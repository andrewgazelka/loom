//! The object-store lease chooses placement; node records only resolve addresses.
use crate::{ActorId, Node, actor, remote_store::Lease};
use anyhow::{Context, Result, ensure};
use object_store::{PutMode, path::Path};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Mutex};

#[derive(Clone, Debug)]
pub struct ClusterConfig {
    /// Empty selects the node id persisted on first open.
    pub node_id: String,
    pub addr: String,
    pub key: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub node_id: String,
    pub addr: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    Local,
    Remote { node_id: String, addr: String },
    Unowned,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterNode {
    pub node_id: String,
    pub addr: String,
    pub started_at: u64,
    pub expires_at: u64,
    pub live: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NodeRecord {
    addr: String,
    started_at: u64,
    expires_at: u64,
}

pub(crate) struct CachedPlacement {
    placement: Placement,
    expires_at: u64,
}

pub(crate) struct ClusterState {
    identity: NodeIdentity,
    started_at: u64,
    // resolve removes expired entries; invalidate_placement removes failed ingress routes.
    placements: Mutex<HashMap<ActorId, CachedPlacement>>,
}

impl ClusterState {
    pub(crate) fn identity(&self) -> NodeIdentity {
        self.identity.clone()
    }

    pub(crate) async fn open(conn: &turso::Connection, config: &ClusterConfig, now: u64) -> Result<Self> {
        ensure!(!config.addr.trim().is_empty(), "--advertise is required with --store");
        let rows = actor::query(conn, "SELECT value FROM meta WHERE key='node_id'", ()).await?;
        let persisted = rows.rows.first().map(|row| row.get::<String>(0)).transpose()?;
        let node_id = if let Some(persisted) = persisted {
            ensure!(
                config.node_id.is_empty() || config.node_id == persisted,
                "--node-id differs from this actors directory's persisted node_id {persisted}"
            );
            persisted
        } else {
            let id = if config.node_id.is_empty() { ulid::Ulid::new().to_string() } else { config.node_id.clone() };
            ensure!(
                !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
                "--node-id must contain only ASCII letters, digits, hyphens or underscores"
            );
            actor::set_meta(conn, "node_id", &id).await?;
            id
        };
        Ok(Self { identity: NodeIdentity { node_id, addr: config.addr.clone() }, started_at: now, placements: Mutex::new(HashMap::new()) })
    }
}

impl Node {
    pub fn identity(&self) -> Option<NodeIdentity> {
        self.cluster.as_ref().map(|cluster| cluster.identity.clone())
    }

    pub fn ingress_bearer(&self) -> Option<String> {
        self.config.cluster.as_ref().map(|config| blake3::keyed_hash(&config.key, b"loom-ingress-v1").to_hex().to_string())
    }

    pub(crate) async fn renew_node(&self) -> Result<()> {
        let Some(cluster) = &self.cluster else { return Ok(()) };
        let store = self.remote.as_ref().context("--store is required with --cluster-key-file")?;
        let now = self.config.lease_clock.now_ms()?;
        let ttl: u64 = self.config.lease_ttl.as_millis().try_into()?;
        let key = format!("nodes/{}", cluster.identity.node_id);
        let mode = match store.read::<NodeRecord>(&key).await? {
            Some(old) => PutMode::Update(old.version),
            None => PutMode::Create,
        };
        let record = NodeRecord {
            addr: cluster.identity.addr.clone(),
            started_at: cluster.started_at,
            expires_at: now.checked_add(ttl).context("node lease expiry overflow")?,
        };
        store.write(&key, &record, mode).await.context("renew node address record")?;
        Ok(())
    }

    pub fn invalidate_placement(&self, id: &str) -> Result<()> {
        if let Some(cluster) = &self.cluster {
            cluster.placements.lock().map_err(|_| anyhow::anyhow!("placement cache poisoned"))?.remove(id);
        }
        Ok(())
    }

    pub async fn resolve(&self, id: &str) -> Result<Placement> {
        // Drivers are node-local resources (drivers.rs): never a placement question.
        if id.starts_with("drv:") {
            return Ok(Placement::Local);
        }
        crate::ids::check(id)?;
        let Some(store) = &self.remote else { return Ok(Placement::Local) };
        // Locally owned actors bypass the placement cache; expiry fences local access.
        match store.check(id) {
            Ok(()) => return Ok(Placement::Local),
            Err(error) if error.downcast_ref::<crate::remote_store::LeaseLost>().is_some() => {}
            Err(error) => return Err(error.context(format!("actor {id} seq -1: resolve local lease"))),
        }
        let cluster = self.cluster.as_ref().context("--advertise and --cluster-key-file are required with --store")?;
        let now = self.config.lease_clock.now_ms()?;
        {
            let mut cache = cluster.placements.lock().map_err(|_| anyhow::anyhow!("placement cache poisoned"))?;
            if let Some(entry) = cache.get(id) {
                if now < entry.expires_at {
                    return Ok(entry.placement.clone());
                }
                cache.remove(id);
            }
        }
        let Some(lease) = store.read::<Lease>(&format!("actors/{id}/lease")).await? else { return Ok(Placement::Unowned) };
        if now >= lease.value.expires_at {
            return Ok(Placement::Unowned);
        }
        if lease.value.owner == cluster.identity.node_id {
            return Ok(Placement::Local);
        }
        let owner = store
            .read::<NodeRecord>(&format!("nodes/{}", lease.value.owner))
            .await?
            .with_context(|| format!("actor {id} seq -1: lease owner {} has no node address", lease.value.owner))?;
        // Address expiry does not change ownership: only the actor lease decides liveness.
        let placement = Placement::Remote { node_id: lease.value.owner, addr: owner.value.addr };
        cluster
            .placements
            .lock()
            .map_err(|_| anyhow::anyhow!("placement cache poisoned"))?
            .insert(id.into(), CachedPlacement { placement: placement.clone(), expires_at: lease.value.expires_at });
        Ok(placement)
    }

    pub fn lease_epoch(&self, id: &str) -> Result<u64> {
        self.remote.as_ref().context("--store is required to inspect a lease epoch")?.epoch(id)
    }

    pub async fn nodes(&self) -> Result<Vec<ClusterNode>> {
        let store = self.remote.as_ref().context("--store is required for nodes")?;
        let now = self.config.lease_clock.now_ms()?;
        let prefix = Path::from("nodes/");
        let mut objects = store.store.list(Some(&prefix));
        let mut nodes = Vec::new();
        while let Some(object) = std::future::poll_fn(|cx| objects.as_mut().poll_next(cx)).await {
            let key = object?.location.to_string();
            let Some(node_id) = key.strip_prefix("nodes/") else { continue };
            let record = store.read::<NodeRecord>(&key).await?.context("listed node record disappeared")?.value;
            nodes.push(ClusterNode {
                node_id: node_id.into(),
                addr: record.addr,
                started_at: record.started_at,
                expires_at: record.expires_at,
                live: now < record.expires_at,
            });
        }
        nodes.sort_by(|left, right| left.node_id.cmp(&right.node_id));
        Ok(nodes)
    }

    pub async fn cluster_actor_ids(&self) -> Result<Vec<ActorId>> {
        let store = self.remote.as_ref().context("--store is required for actors --cluster")?;
        let prefix = Path::from("actors/");
        let mut objects = store.store.list(Some(&prefix));
        let mut ids = std::collections::BTreeSet::new();
        while let Some(object) = std::future::poll_fn(|cx| objects.as_mut().poll_next(cx)).await {
            let key = object?.location.to_string();
            if let Some(id) = key.strip_prefix("actors/").and_then(|key| key.strip_suffix("/lease")) {
                crate::ids::check(id)?;
                ids.insert(id.to_owned());
            }
        }
        Ok(ids.into_iter().collect())
    }
}
