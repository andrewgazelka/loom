//! Ephemeral resource owners. The pump is their only outbound delivery path.
mod receipts;
mod runtime;
pub use receipts::{DriverReceipt, DriverReceipts};
pub mod container;
pub mod process;
pub mod vm;
pub mod tcp;

use crate::{Cap, Ctx, Rights, Trap, cap_ops::Operation};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

pub(crate) use runtime::{DriverLifetime, Drivers};

/// A native definition, resolved by hash before spawn commits. `run` owns all
/// resources and child futures: dropping it MUST close them. Do not detach tasks.
/// A return or panic closes the driver and sends its owner one monitored DOWN.
#[async_trait]
pub trait Driver: Send + Sync {
    fn hash(&self) -> &str;
    async fn run(&self, cx: DriverContext, init: &[u8], deliveries: mpsc::Receiver<DriverDelivery>) -> Result<()>;
}

/// Acked deliveries leave the pump through its normal delivered-row update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverAck {
    Delivered,
    /// Terminal: the resource is gone. The pump records `driver_drop:<key>`.
    Dropped,
}

/// One committed send. Dedupe `key` before performing resource I/O; an error
/// blocks only this handle's later rows. Dropping without ack is an error.
pub struct DriverDelivery {
    pub handle: String,
    pub key: String,
    pub bytes: Vec<u8>,
    pub(crate) ack: oneshot::Sender<Result<DriverAck>>,
}
impl DriverDelivery {
    pub fn acknowledge(self, result: Result<DriverAck>) {
        // A cancelled pump retries with the same key.
        let _ = self.ack.send(result);
    }
}

pub(crate) struct Injection {
    sender: String,
    cap: Cap,
    key: String,
    bytes: Vec<u8>,
    ack: oneshot::Sender<Result<()>>,
}

/// Only `inject` crosses into the kernel. A private channel keeps driver code
/// outside transactions; cancelling a driver cannot cancel a kernel COMMIT.
#[derive(Clone)]
pub struct DriverContext {
    driver_id: String,
    handle: String,
    owner: Cap,
    inject: mpsc::Sender<Injection>,
}
impl DriverContext {
    pub fn owner(&self) -> &Cap {
        &self.owner
    }
    pub fn id(&self) -> &str {
        &self.driver_id
    }

    /// Pure sender scoping. Handles cannot contain ':'; `root` names the driver.
    /// A reply capability is minted in the recipient's transaction on injection.
    pub fn for_handle(&self, handle: &str) -> Result<Self> {
        ensure!(!handle.is_empty() && !handle.contains(':'), "invalid driver handle {handle:?}");
        Ok(Self { handle: handle.into(), ..self.clone() })
    }

    /// Exactly-once inbox insert by the caller's resource-derived key. Keys are
    /// unique across the destination inbox, including previous driver instances.
    pub async fn inject(&self, cap: &Cap, key: &str, bytes: &[u8]) -> Result<()> {
        let (ack, result) = oneshot::channel();
        self.inject
            .send(Injection {
                sender: format!("drv:{}:{}", self.driver_id, self.handle),
                cap: cap.clone(),
                key: key.into(),
                bytes: bytes.into(),
                ack,
            })
            .await
            .context("driver kernel closed")?;
        result.await.context("driver injection acknowledgement lost")?
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct DriverSpawn {
    pub id: String,
    pub hash: String,
    pub init: Vec<u8>,
    pub owner: Cap,
}

pub(crate) struct Target<'a> {
    pub owner: &'a str,
    pub id: &'a str,
    pub handle: &'a str,
}
pub(crate) fn target(value: &str) -> Result<Target<'_>> {
    let rest = value.strip_prefix("drv:").context("missing drv namespace")?;
    let (id, handle) = rest.split_once(':').context("missing driver handle")?;
    let (owner, local) = id.split_once('/').context("missing driver owner")?;
    crate::ids::check(owner)?;
    crate::ids::check(local)?;
    ensure!(!handle.is_empty() && !handle.contains(':'), "invalid driver handle");
    Ok(Target { owner, id, handle })
}

impl Ctx<'_> {
    /// Resolve a native driver hash, then enqueue its linked spawn. Resources open
    /// only after commit. A stopped driver leaves through a NEW spawn by its owner.
    pub async fn spawn_driver(&mut self, def_hash: &str, init: &[u8]) -> Result<Cap, Trap> {
        let idx = self.next_index()?;
        let local = crate::ids::child(&crate::ids::incarnation(self.actor_id, self.generation), self.seq, idx);
        let id = format!("{}/{local}", self.actor_id);
        let target = format!("drv:{id}:root");
        let bytes = self.cap_operation(Operation::DriverSpawn { target: target.clone(), hash: def_hash.into() }).await?;
        let cap: Cap = serde_json::from_slice(&bytes).map_err(|e| self.runtime(e))?;
        crate::capability::store_cap(self.conn, &cap).await.map_err(|e| self.runtime(e))?;
        let own = self.self_cap().await?;
        let owner = self.attenuate(&own, Rights::SEND).await?;
        let spawn = DriverSpawn { id, hash: def_hash.into(), init: init.into(), owner };
        let bytes = serde_json::to_vec(&spawn).map_err(|e| self.runtime(e))?;
        self.outbox(idx, &format!("drv:spawn:{target}"), &bytes).await?;
        Ok(cap)
    }

    /// Authority supplied with a driver message, stored atomically with its inbox
    /// row. Ordinary actor messages should continue to carry explicit reply caps.
    pub async fn sender_cap(&mut self) -> Result<Cap, Trap> {
        let sender = self.sender.clone().ok_or_else(|| Trap::new("message has no sender"))?;
        let bytes = self.cap_operation(Operation::SenderCap { sender }).await?;
        let cap: Cap = serde_json::from_slice(&bytes).map_err(|e| self.runtime(e))?;
        crate::capability::store_cap(self.conn, &cap).await.map_err(|e| self.runtime(e))?;
        Ok(cap)
    }
}
