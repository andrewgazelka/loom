//! Capability calls cross the same recording/interception boundary as effects.
use crate::{Cap, ChildState, Ctx, EffectError, EffectKey, Node, Rights, Trap, actor, capability};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation")]
pub(crate) enum Operation {
    PrepareSpawn { spec: crate::ChildSpec },
    Check { cap: Cap, right: Rights, name: String },
    SelfCap,
    ResolveName { name: String },
    ResolveProcess { name: String },
    SenderCap { sender: String },
    DriverSpawn { target: String, hash: String },
    Spawn { target: String },
    Attenuate { cap: Cap, rights: Rights },
    Accept { cap: Cap },
    Revoke { cap: Cap },
    Inspect { cap: Cap },
    InspectSql { cap: Cap, query: String, params: Vec<crate::SqlValue> },
}

struct Presented<'a> {
    cap: &'a Cap,
    name: &'a str,
}
impl Operation {
    fn presented(&self) -> Option<Presented<'_>> {
        Some(match self {
            Self::Check { cap, name, .. } => Presented { cap, name },
            Self::Attenuate { cap, .. } => Presented { cap, name: "attenuate" },
            Self::Accept { cap } => Presented { cap, name: "accept" },
            Self::Revoke { cap } => Presented { cap, name: "revoke" },
            Self::Inspect { cap } => Presented { cap, name: "inspect" },
            Self::InspectSql { cap, .. } => Presented { cap, name: "inspect_sql" },
            Self::SelfCap
            | Self::ResolveName { .. }
            | Self::ResolveProcess { .. }
            | Self::SenderCap { .. }
            | Self::Spawn { .. }
            | Self::PrepareSpawn { .. }
            | Self::DriverSpawn { .. } => {
                return None;
            }
        })
    }
}

impl Ctx<'_> {
    fn deterministic(&mut self, message: impl Into<String>) -> Trap {
        self.effect_error(EffectError::Deterministic(anyhow::anyhow!(message.into())))
    }

    pub(crate) fn check_external_effect(&mut self, kind: &str) -> Result<(), Trap> {
        if kind.starts_with("__") {
            return Err(self.deterministic(format!("effect {kind}: reserved host operation")));
        }
        Ok(())
    }

    pub(crate) async fn cap_operation(&mut self, operation: Operation) -> Result<Vec<u8>, Trap> {
        let request = serde_json::to_vec(&operation).map_err(|e| self.runtime(e))?;
        let idx = self.next_index()?;
        let key = EffectKey { actor_id: self.actor_id.into(), seq: self.seq, idx, generation: self.generation };
        let result = self.effects.capability(&key, self.conn, &request).await.map_err(|e| self.effect_error(e))?;
        self.conn
            .execute(
                "INSERT INTO effects(seq,idx,kind,request,result) VALUES (?,?,'__cap',?,?)",
                turso::params![self.seq, idx, request, result.as_slice()],
            )
            .await
            .map_err(|e| self.runtime(e))?;
        Ok(result)
    }

    pub(crate) async fn authorize(&mut self, cap: &Cap, right: Rights, name: &str) -> Result<(), Trap> {
        self.cap_operation(Operation::Check { cap: cap.clone(), right, name: name.into() }).await?;
        Ok(())
    }

    async fn minted(&mut self, operation: Operation) -> Result<Cap, Trap> {
        let bytes = self.cap_operation(operation).await?;
        let cap = serde_json::from_slice(&bytes).map_err(|e| self.runtime(e))?;
        capability::store_cap(self.conn, &cap).await.map_err(|e| self.runtime(e))?;
        Ok(cap)
    }

    pub(crate) async fn mint_child(&mut self, target: &str) -> Result<Cap, Trap> {
        self.minted(Operation::Spawn { target: target.into() }).await
    }

    /// Authority over this actor only; never resolves another actor's identity.
    pub async fn self_cap(&mut self) -> Result<Cap, Trap> {
        self.minted(Operation::SelfCap).await
    }

    /// Resolve a host-published name inside this Node's tenant directory.
    /// Publication grants SEND only; knowing an arbitrary actor ID grants nothing.
    /// The capability result is recorded and retained in this actor's held caps.
    pub async fn resolve_name(&mut self, name: &str) -> Result<Cap, Trap> {
        self.minted(Operation::ResolveName { name: name.into() }).await
    }

    pub async fn cap(&mut self, cap_id: u64) -> Result<Cap, Trap> {
        capability::load_cap(self.conn, cap_id).await.map_err(|e| self.deterministic(format!("cap cap_id {cap_id}: {e}")))
    }

    pub async fn attenuate(&mut self, cap: &Cap, rights: Rights) -> Result<Cap, Trap> {
        self.minted(Operation::Attenuate { cap: cap.clone(), rights }).await
    }

    pub async fn accept(&mut self, cap: Cap) -> Result<(), Trap> {
        self.cap_operation(Operation::Accept { cap: cap.clone() }).await?;
        capability::store_cap(self.conn, &cap).await.map_err(|e| self.runtime(e))
    }

    pub async fn revoke(&mut self, cap_id: u64) -> Result<(), Trap> {
        let cap = capability::load_cap(self.conn, cap_id).await.map_err(|e| self.deterministic(format!("revoke cap_id {cap_id}: {e}")))?;
        self.cap_operation(Operation::Revoke { cap: cap.clone() }).await?;
        let bytes = serde_json::to_vec(&cap).map_err(|e| self.runtime(e))?;
        self.control("revoke", &cap.target, &bytes).await
    }

    pub async fn inspect(&mut self, cap: &Cap) -> Result<ChildState, Trap> {
        let bytes = self.cap_operation(Operation::Inspect { cap: cap.clone() }).await?;
        serde_json::from_slice(&bytes).map_err(|e| self.runtime(e))
    }

    pub async fn promote(&mut self, cap: &Cap, hash: &str, author: &str, rationale: &str) -> Result<(), Trap> {
        self.authorize(cap, Rights::PROMOTE, "promote").await?;
        let bytes = serde_json::to_vec(&Promotion { hash: hash.into(), author: author.into(), rationale: rationale.into() })
            .map_err(|e| self.runtime(e))?;
        self.control("promote", &cap.target, &bytes).await
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Promotion {
    pub hash: String,
    pub author: String,
    pub rationale: String,
}

async fn verify(node: &Node, conn: &turso::Connection, cap: &Cap, right: Rights, name: &str) -> Result<(), EffectError> {
    let id = actor::meta(conn, "id").await.map_err(EffectError::Environmental)?;
    if id == cap.target
        || (cap.target.starts_with("drv:") && crate::drivers::target(&cap.target).map_err(EffectError::Deterministic)?.owner == id)
    {
        return node.verify_cap_on(conn, cap, right, name).await;
    }
    // A child capability is usable in the spawning transaction, before the pump
    // creates its file. The parent transaction is the owner of this pending state.
    let spawns =
        actor::query(conn, "SELECT msg FROM outbox WHERE target='spawn' AND delivered=0", ()).await.map_err(EffectError::Environmental)?;
    let mut pending = false;
    for row in spawns.rows {
        let bytes: Vec<u8> = row.get(0).map_err(|error| EffectError::Environmental(error.into()))?;
        let spawn: crate::Spawn = serde_json::from_slice(&bytes).map_err(|error| EffectError::Environmental(error.into()))?;
        if matches!(spawn, crate::Spawn::Child { id, .. } if id == cap.target) {
            pending = true;
            break;
        }
    }
    let unowned = node.config.store.is_none()
        || matches!(node.resolve(&cap.target).await.map_err(EffectError::Environmental)?, crate::Placement::Unowned);
    if pending && let Some(store) = &node.remote {
        // A published head means this child existed already; restore and verify its revocations.
        pending =
            store.read::<serde_json::Value>(&format!("actors/{}/head", cap.target)).await.map_err(EffectError::Environmental)?.is_none();
    }
    if pending && unowned && !node.path(&cap.target).exists() && !node.connections.lock().await.contains_key(&cap.target) {
        node.verify_cap_mac(cap, right, name)?;
        if cap.epoch != 0 {
            return Err(EffectError::Deterministic(anyhow::anyhow!(
                "invalid authority: {name} cap_id {}: pending child epoch must be zero",
                cap.cap_id
            )));
        }
        return Ok(());
    }
    node.verify_cap(cap, right, name).await
}

pub(crate) async fn execute(node: &Node, conn: &turso::Connection, key: &EffectKey, request: &[u8]) -> Result<Vec<u8>, EffectError> {
    let operation: Operation = serde_json::from_slice(request).map_err(|e| EffectError::Deterministic(e.into()))?;
    if let Some(presented) = operation.presented() {
        node.verify_cap_mac(presented.cap, Rights::NONE, presented.name)?;
        let revocations =
            actor::query(conn, "SELECT msg FROM outbox WHERE target=? AND delivered=0", [format!("revoke:{}", presented.cap.target)])
                .await
                .map_err(EffectError::Environmental)?;
        for row in revocations.rows {
            let bytes: Vec<u8> = row.get(0).map_err(|e| EffectError::Environmental(e.into()))?;
            let revoked: Cap = serde_json::from_slice(&bytes).map_err(|e| EffectError::Environmental(e.into()))?;
            if revoked.cap_id == presented.cap.cap_id {
                return Err(EffectError::Deterministic(anyhow::anyhow!(
                    "invalid authority: {} cap_id {}: revoked in this transaction",
                    presented.name,
                    presented.cap.cap_id
                )));
            }
        }
    }
    let identity = format!("cap:{}:{}:{}:{}", key.actor_id, key.generation, key.seq, key.idx);
    let cap = match operation {
        Operation::PrepareSpawn { spec } => {
            let spec = crate::view::pin_spec(&node.registry, &spec).await.map_err(EffectError::Deterministic)?;
            return serde_json::to_vec(&spec).map_err(|e| EffectError::Environmental(e.into()));
        }
        Operation::Check { cap, right, name } => {
            verify(node, conn, &cap, right, &name).await?;
            return Ok(Vec::new());
        }
        Operation::SenderCap { sender } => {
            let rows = actor::query(conn, "SELECT cap_id FROM caps WHERE target=? ORDER BY rowid DESC LIMIT 1", [sender])
                .await
                .map_err(EffectError::Environmental)?;
            let row =
                rows.rows.first().ok_or_else(|| EffectError::Deterministic(anyhow::anyhow!("sender supplied no reply capability")))?;
            let id: i64 = row.get(0).map_err(|e| EffectError::Environmental(e.into()))?;
            capability::load_cap(conn, id as u64).await.map_err(EffectError::Environmental)?
        }
        Operation::ResolveProcess { name } => {
            let hash = node.registry.resolve_process(&name).await.map_err(EffectError::Deterministic)?;
            return serde_json::to_vec(&hash).map_err(|error| EffectError::Environmental(error.into()));
        }
        Operation::ResolveName { name } => node.resolve_name_cap(conn, &key.actor_id, &name, identity.as_bytes()).await?,
        Operation::SelfCap => {
            let epoch = actor::meta(conn, "capability_epoch")
                .await
                .map_err(EffectError::Environmental)?
                .parse()
                .map_err(|e| EffectError::Environmental(anyhow::anyhow!("invalid capability_epoch: {e}")))?;
            node.mint_cap_at(&key.actor_id, epoch, identity.as_bytes())
        }
        Operation::DriverSpawn { target, hash } => {
            let driver = node
                .registry
                .resolve_driver(&hash)
                .await
                .map_err(|e| EffectError::Deterministic(anyhow::anyhow!("unknown driver hash {hash}: {e:#}")))?;
            if driver.hash() != hash {
                return Err(EffectError::Deterministic(anyhow::anyhow!("driver hash {hash}: registry identity mismatch")));
            }
            let epoch: u64 = actor::meta(conn, "capability_epoch")
                .await
                .map_err(EffectError::Environmental)?
                .parse()
                .map_err(|e| EffectError::Environmental(anyhow::anyhow!("invalid capability epoch: {e}")))?;
            let cap = node.mint_cap_at(&target, epoch, identity.as_bytes());
            node.attenuate_verified(&cap, Rights::SEND | Rights::STOP)?
        }
        Operation::Spawn { target } => node.mint_child_cap(&target, identity.as_bytes()),
        Operation::Attenuate { cap, rights } => {
            verify(node, conn, &cap, Rights::NONE, "attenuate").await?;
            node.attenuate_verified(&cap, rights)?
        }
        Operation::Accept { cap } => {
            verify(node, conn, &cap, Rights::NONE, "accept").await?;
            return Ok(Vec::new());
        }
        Operation::Revoke { cap } => {
            verify(node, conn, &cap, Rights::NONE, "revoke").await?;
            return Ok(Vec::new());
        }
        Operation::Inspect { cap } => {
            return crate::cap_inspection::state(node, conn, key, &cap).await;
        }
        Operation::InspectSql { cap, query, params } => {
            return crate::cap_inspection::query(node, conn, key, &cap, &query, params).await;
        }
    };
    serde_json::to_vec(&cap).map_err(|e| EffectError::Environmental(e.into()))
}
