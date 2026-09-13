//! Node-authenticated actor authority. Only the host owns the signing key.
use crate::{ActorId, EffectError, Node, actor};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::io::Read;
use turso::Connection;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rights {
    pub bits: u64,
}
impl Rights {
    pub const SEND: Self = Self { bits: 1 };
    pub const SPAWN: Self = Self { bits: 2 };
    pub const STOP: Self = Self { bits: 4 };
    pub const MONITOR: Self = Self { bits: 8 };
    pub const LINK: Self = Self { bits: 16 };
    pub const PROMOTE: Self = Self { bits: 32 };
    pub const INSPECT: Self = Self { bits: 64 };
    pub const ALL: Self = Self { bits: 127 };
    pub const NONE: Self = Self { bits: 0 };
    pub const fn contains(self, other: Self) -> bool {
        self.bits & other.bits == other.bits
    }
    pub const fn is_subset(self, other: Self) -> bool {
        other.contains(self)
    }
}
impl std::ops::BitOr for Rights {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self { bits: self.bits | rhs.bits }
    }
}
impl std::ops::BitAnd for Rights {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self { bits: self.bits & rhs.bits }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cap {
    pub target: ActorId,
    pub cap_id: u64,
    pub epoch: u64,
    pub rights: Rights,
    pub mac: [u8; 32],
}

pub(crate) async fn node_key(conn: &Connection) -> Result<[u8; 32]> {
    conn.execute("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT)", ()).await?;
    let mut entropy = [0; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut entropy)?;
    conn.execute("INSERT OR IGNORE INTO meta(key,value) VALUES ('capability_key',?)", [serde_json::to_string(&entropy)?]).await?;
    serde_json::from_str(&actor::meta(conn, "capability_key").await?).context("node capability key must contain 32 bytes")
}

pub(crate) async fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS caps(cap_id INTEGER PRIMARY KEY,target TEXT NOT NULL,epoch TEXT NOT NULL,rights INTEGER NOT NULL,mac BLOB NOT NULL); CREATE TABLE IF NOT EXISTS revoked(cap_id INTEGER PRIMARY KEY); INSERT OR IGNORE INTO meta(key,value) VALUES ('capability_epoch','0');").await?;
    Ok(())
}

pub(crate) async fn store_cap(conn: &Connection, cap: &Cap) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO caps(cap_id,target,epoch,rights,mac) VALUES (?,?,?,?,?)",
        turso::params![cap.cap_id as i64, cap.target.as_str(), cap.epoch.to_string(), cap.rights.bits as i64, cap.mac.as_slice()],
    )
    .await?;
    ensure!(load_cap(conn, cap.cap_id).await? == *cap, "cap_id {}: stored token identity collision", cap.cap_id);
    Ok(())
}
pub(crate) async fn load_cap(conn: &Connection, cap_id: u64) -> Result<Cap> {
    let rows = actor::query(conn, "SELECT target,epoch,rights,mac FROM caps WHERE cap_id=?", [cap_id as i64]).await?;
    let row = rows.rows.first().with_context(|| format!("capability {cap_id} is not held"))?;
    Ok(Cap {
        target: row.get(0)?,
        cap_id,
        epoch: row.get::<String>(1)?.parse()?,
        rights: Rights { bits: row.get::<i64>(2)? as u64 },
        mac: row.get::<Vec<u8>>(3)?.try_into().map_err(|_| anyhow::anyhow!("capability {cap_id}: invalid MAC length"))?,
    })
}

pub(crate) enum CapabilityReader {
    Locked { conn: tokio::sync::OwnedMutexGuard<Connection> },
    Independent { conn: Connection },
}
impl std::ops::Deref for CapabilityReader {
    type Target = Connection;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Locked { conn } => conn,
            Self::Independent { conn } => conn,
        }
    }
}

impl Node {
    pub(crate) async fn migrate_authority(&self, conn: &Connection) -> Result<()> {
        if actor::query(conn, "SELECT value FROM meta WHERE key='shutdown'", ()).await?.rows.is_empty() {
            let id = actor::meta(conn, "id").await?;
            let replay = actor::query(conn, "SELECT value FROM meta WHERE key='replay_source'", ()).await?;
            let source;
            let policy_owner = if let Some(row) = replay.rows.first() {
                let source_id = row.get::<String>(0)?;
                crate::ids::check(&source_id)?;
                ensure!(source_id != id, "actor {id}: shutdown migration replay source points to itself");
                let path = self.path(&source_id);
                ensure!(path.is_file(), "actor {id}: shutdown migration missing replay source {source_id}");
                source = actor::connect(&path, self.config.io).await?;
                ensure!(actor::meta(&source, "id").await? == source_id, "actor {id}: shutdown migration replay source identity mismatch");
                ensure!(
                    actor::query(&source, "SELECT value FROM meta WHERE key='replay_source'", ()).await?.rows.is_empty(),
                    "actor {id}: shutdown migration replay source {source_id} is not the original actor"
                );
                &source
            } else {
                conn
            };
            let existing = actor::query(policy_owner, "SELECT value FROM meta WHERE key='shutdown'", ()).await?;
            let policy = if let Some(row) = existing.rows.first() {
                row.get::<String>(0)?
            } else {
                let owner_id = actor::meta(policy_owner, "id").await?;
                let parent = actor::meta(policy_owner, "parent").await?;
                if parent.is_empty() {
                    serde_json::to_string(&crate::Shutdown::Infinity)?
                } else {
                    // Read persisted parents directly: node opens would recursively
                    // migrate while the actor being migrated is locked.
                    let path = self.path(&parent);
                    ensure!(path.is_file(), "actor {id}: shutdown migration missing parent {parent}");
                    let owner = actor::connect(&path, self.config.io).await?;
                    let rows = actor::query(&owner, "SELECT shutdown FROM children WHERE id=?", [owner_id.as_str()]).await?;
                    rows.rows
                        .first()
                        .with_context(|| format!("actor {id}: shutdown migration missing child spec {owner_id} in parent {parent}"))?
                        .get::<String>(0)?
                }
            };
            let _: crate::Shutdown =
                serde_json::from_str(&policy).with_context(|| format!("actor {id}: invalid shutdown policy during migration"))?;
            actor::set_meta(conn, "shutdown", &policy).await?;
        }
        let columns = actor::query(conn, "PRAGMA table_info(spec)", ()).await?;
        if !columns.rows.is_empty() && !columns.rows.iter().any(|row| row.get::<String>(1).is_ok_and(|name| name == "cap")) {
            conn.execute("ALTER TABLE spec ADD COLUMN cap TEXT", ()).await?;
        }
        if !columns.rows.is_empty() {
            let children = actor::query(conn, "SELECT child_id FROM spec WHERE cap IS NULL", ()).await?;
            for row in children.rows {
                let id: String = row.get(0)?;
                let cap = self.mint_child_cap(&id, id.as_bytes());
                store_cap(conn, &cap).await?;
                conn.execute("UPDATE spec SET cap=? WHERE child_id=?", [serde_json::to_string(&cap)?, id]).await?;
            }
            ensure!(
                actor::query(conn, "SELECT child_id FROM spec WHERE cap IS NULL", ()).await?.rows.is_empty(),
                "supervisor capability migration incomplete"
            );
        }
        let journals = actor::query(conn, "SELECT key,value FROM meta WHERE key LIKE 'host_spawn:%'", ()).await?;
        for row in journals.rows {
            let key: String = row.get(0)?;
            let mut value: serde_json::Value = serde_json::from_str(&row.get::<String>(1)?)?;
            if value.get("cap").is_none() {
                let id = value.get("child_id").and_then(serde_json::Value::as_str).context("host spawn journal missing child_id")?;
                let cap = self.mint_child_cap(id, id.as_bytes());
                value["cap"] = serde_json::to_value(&cap)?;
                actor::set_meta(conn, &key, &serde_json::to_string(&value)?).await?;
            }
        }
        Ok(())
    }

    fn sign_cap(&self, cap: &mut Cap) {
        let mut bytes = cap.target.as_bytes().to_vec();
        bytes.extend_from_slice(&cap.cap_id.to_le_bytes());
        bytes.extend_from_slice(&cap.epoch.to_le_bytes());
        bytes.extend_from_slice(&cap.rights.bits.to_le_bytes());
        cap.mac = *blake3::keyed_hash(&self.capability_key, &bytes).as_bytes();
    }
    pub(crate) fn mint_child_cap(&self, target: &str, identity: &[u8]) -> Cap {
        self.mint_cap_at(target, 0, identity)
    }
    pub(crate) fn mint_cap_at(&self, target: &str, epoch: u64, identity: &[u8]) -> Cap {
        let digest = blake3::keyed_hash(&self.capability_key, identity);
        let mut id = [0; 8];
        id.copy_from_slice(&digest.as_bytes()[..8]);
        let mut cap = Cap { target: target.into(), cap_id: u64::from_le_bytes(id), epoch, rights: Rights::ALL, mac: [0; 32] };
        self.sign_cap(&mut cap);
        cap
    }
    pub async fn cap_for(&self, id: &str, rights: Rights) -> Result<Cap> {
        let _admission = self.admit().await?;
        ensure!(Rights::ALL.contains(rights), "cap_for actor {id}: unknown rights {}", rights.bits);
        let actor = self.open_actor(id).await?;
        let conn = actor.conn.lock().await;
        let mut entropy = [0; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut entropy)?;
        let mut cap = self.mint_child_cap(id, &entropy);
        cap.epoch = actor::meta(&conn, "capability_epoch").await?.parse()?;
        cap.rights = rights;
        self.sign_cap(&mut cap);
        Ok(cap)
    }
    /// Invalidates every delegated capability for this actor by advancing its epoch.
    /// This is the actor-wide invalidation mechanism; reset preserves capability authority.
    pub async fn bump_epoch(&self, id: &str) -> Result<()> {
        let _admission = self.admit().await?;
        let actor = self.open_actor(id).await?;
        let mut conn = actor.conn.lock().await;
        let tx = conn.transaction().await?;
        let epoch = actor::meta(&tx, "capability_epoch").await?.parse::<u64>()?.checked_add(1).context("capability epoch exhausted")?;
        actor::set_meta(&tx, "capability_epoch", &epoch.to_string()).await?;
        self.commit_control(id, tx).await
    }
    pub(crate) fn verify_cap_mac(&self, cap: &Cap, right: Rights, operation: &str) -> Result<(), EffectError> {
        let result = (|| {
            let mut expected = cap.clone();
            self.sign_cap(&mut expected);
            let mismatch = (0..32).fold(0u8, |difference, index| difference | (expected.mac[index] ^ cap.mac[index]));
            ensure!(mismatch == 0, "invalid MAC");
            ensure!(Rights::ALL.contains(cap.rights), "unknown rights");
            ensure!(cap.rights.contains(right), "missing right {}", right.bits);
            Ok(())
        })();
        result.with_context(|| format!("{operation} cap_id {}", cap.cap_id)).map_err(EffectError::Deterministic)
    }
    pub(crate) async fn verify_cap_on(&self, conn: &Connection, cap: &Cap, right: Rights, operation: &str) -> Result<(), EffectError> {
        self.verify_cap_mac(cap, right, operation)?;
        let environmental = |error: anyhow::Error| EffectError::Environmental(error.context(format!("{operation} cap_id {}", cap.cap_id)));
        let target = actor::meta(conn, "id").await.map_err(environmental)?;
        let epoch = actor::meta(conn, "capability_epoch")
            .await
            .map_err(environmental)?
            .parse::<u64>()
            .map_err(|error| environmental(error.into()))?;
        let revoked = actor::query(conn, "SELECT cap_id FROM revoked WHERE cap_id=?", [cap.cap_id as i64]).await.map_err(environmental)?;
        let result = (|| {
            ensure!(target == cap.target, "target identity mismatch");
            ensure!(epoch == cap.epoch, "revoked epoch");
            ensure!(revoked.rows.is_empty(), "revoked capability");
            Ok(())
        })();
        result.with_context(|| format!("{operation} cap_id {}", cap.cap_id)).map_err(EffectError::Deterministic)
    }

    pub async fn check_cap(&self, cap: &Cap, right: Rights, operation: &str) -> Result<()> {
        let _admission = self.admit().await?;
        self.verify_cap(cap, right, operation).await.map_err(anyhow::Error::from)
    }
    pub(crate) async fn verify_cap(&self, cap: &Cap, right: Rights, operation: &str) -> Result<(), EffectError> {
        self.verify_cap_mac(cap, right, operation)?;
        let reader = self.capability_reader(&cap.target).await.map_err(|error| match error {
            EffectError::Environmental(error) => EffectError::Environmental(error.context(format!("{operation} cap_id {}", cap.cap_id))),
            EffectError::Deterministic(error) => EffectError::Deterministic(error.context(format!("{operation} cap_id {}", cap.cap_id))),
        })?;
        self.verify_cap_on(&reader, cap, right, operation).await
    }
    pub(crate) async fn capability_reader(&self, target: &str) -> Result<CapabilityReader, EffectError> {
        let cached = self.connections.lock().await.get(target).cloned();
        let connection = match cached {
            Some(connection) => connection,
            None => self.open_actor(target).await.map_err(EffectError::Environmental)?.conn,
        };
        self.check_lease(target).map_err(EffectError::Environmental)?;
        if let Ok(conn) = connection.try_lock_owned() {
            return Ok(CapabilityReader::Locked { conn });
        }
        if self.is_memory(target).map_err(EffectError::Environmental)? {
            return Err(EffectError::Environmental(anyhow::anyhow!("capability target {target}: target is busy")));
        }
        let conn = actor::connect(&self.path(target), self.config.io).await.map_err(EffectError::Environmental)?;
        Ok(CapabilityReader::Independent { conn })
    }
    pub(crate) fn attenuate_verified(&self, cap: &Cap, rights: Rights) -> Result<Cap, EffectError> {
        if !cap.rights.contains(rights) {
            return Err(EffectError::Deterministic(anyhow::anyhow!("attenuate cap_id {}: rights are not a subset", cap.cap_id)));
        }
        let mut identity = b"attenuate".to_vec();
        identity.extend_from_slice(&cap.mac);
        identity.extend_from_slice(&rights.bits.to_le_bytes());
        let mut attenuated = self.mint_cap_at(&cap.target, cap.epoch, &identity);
        attenuated.rights = rights;
        self.sign_cap(&mut attenuated);
        Ok(attenuated)
    }
    pub(crate) async fn revoke_cap(&self, cap: &Cap) -> Result<(), EffectError> {
        self.verify_cap_mac(cap, Rights::NONE, "revoke")?;
        let actor = self.open_actor(&cap.target).await.map_err(EffectError::Environmental)?;
        let mut conn = actor.conn.lock().await;
        let tx = conn.transaction().await.map_err(|error| EffectError::Environmental(error.into()))?;
        tx.execute("INSERT OR IGNORE INTO revoked(cap_id) VALUES (?)", [cap.cap_id as i64])
            .await
            .map_err(|error| EffectError::Environmental(error.into()))?;
        self.commit_control(&cap.target, tx).await.map_err(EffectError::Environmental)
    }
}
