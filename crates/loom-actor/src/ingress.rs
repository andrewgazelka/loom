//! One destination dispatcher for the pump and authenticated HTTP ingress.
use crate::{Cap, Node, Placement, Rights, SqlValue, actor};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
pub struct OutboxDelivery {
    pub sender: String,
    pub generation: i64,
    pub seq: i64,
    pub idx: i64,
    pub target: String,
    pub key: String,
    pub msg: Vec<u8>,
}

// Mirrors pump.rs at c4eef69, destination:180 and deliver_outbox:127:
// spawn child/restart:130/149; call:159; effect/alarm:161/162;
// message (including call replies through complete_call:19):174; relations:168, expanded by
// supervision.rs:12 revoke, 17 promote, 21 stop, 22 shutdown, 23 link/unlink,
// 43 monitor, 44 demonitor, 59 down/exit. No catch-all relation is admitted.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryOp {
    Message { target: String, key: String, sender: String, msg: Vec<u8> },
    Spawn { delivery: OutboxDelivery },
    Call { delivery: OutboxDelivery },
    Effect { delivery: OutboxDelivery },
    Revoke { delivery: OutboxDelivery },
    Promote { delivery: OutboxDelivery },
    Stop { delivery: OutboxDelivery },
    Shutdown { delivery: OutboxDelivery },
    Link { delivery: OutboxDelivery },
    Unlink { delivery: OutboxDelivery },
    Monitor { delivery: OutboxDelivery },
    Demonitor { delivery: OutboxDelivery },
    Down { delivery: OutboxDelivery },
    Exit { delivery: OutboxDelivery },
    // View-actor kinds (docs/ui-view-actor.md): `sub:<actor>` subscription ops applied on the
    // source, `frame:<actor>` delta frames into a subscriber's inbox, `ws:<conn>` frames to a
    // host-side WebSocket subscriber (local to the sender's node).
    Subscription { delivery: OutboxDelivery },
    Frame { delivery: OutboxDelivery },
    Stream { delivery: OutboxDelivery },
    // `drv:` rows drive a node-local native resource owned by the sender (drivers.rs).
    Driver { delivery: OutboxDelivery },
    Publish { target: String },
    Relationship { target: String, write: crate::relation_delivery::RelationshipWrite },
    Release { target: String },
    Adopt { target: String },
    HostSpawn { target: String, spec: crate::ChildSpec },
    State { target: String },
    Children { target: String },
    Authority { target: String, cap: Cap, right: Rights, operation: String },
    Mint { target: String, rights: Rights },
    Inspect { target: String, cap: Cap, query: Option<String>, params: Vec<SqlValue> },
    Command { target: String, verb: String, args: Value },
}

impl DeliveryOp {
    pub fn target(&self) -> Result<String> {
        if let Some(delivery) = self.outbox() {
            let expected = match self {
                Self::Spawn { .. } => "spawn",
                Self::Call { .. } => "call",
                Self::Effect { .. } => "effect",
                Self::Revoke { .. } => "revoke",
                Self::Promote { .. } => "promote",
                Self::Stop { .. } => "stop",
                Self::Shutdown { .. } => "shutdown",
                Self::Link { .. } => "link",
                Self::Unlink { .. } => "unlink",
                Self::Monitor { .. } => "monitor",
                Self::Demonitor { .. } => "demonitor",
                Self::Down { .. } => "down",
                Self::Exit { .. } => "exit",
                Self::Subscription { .. } => "sub",
                Self::Frame { .. } => "frame",
                Self::Stream { .. } => "ws",
                Self::Driver { .. } => "drv",
                _ => anyhow::bail!("delivery payload without destination kind"),
            };
            ensure!(
                delivery.target.split(':').next() == Some(expected),
                "actor {} seq {}: ingress destination kind mismatch",
                delivery.sender,
                delivery.seq
            );
        }
        Ok(match self {
            Self::Message { target, .. }
            | Self::Publish { target }
            | Self::Relationship { target, .. }
            | Self::Release { target }
            | Self::Adopt { target }
            | Self::State { target }
            | Self::Children { target }
            | Self::HostSpawn { target, .. }
            | Self::Authority { target, .. }
            | Self::Mint { target, .. }
            | Self::Inspect { target, .. }
            | Self::Command { target, .. } => target.clone(),
            Self::Call { delivery }
            | Self::Effect { delivery }
            | Self::Demonitor { delivery }
            | Self::Link { delivery }
            | Self::Unlink { delivery }
            | Self::Monitor { delivery }
            | Self::Stream { delivery }
            | Self::Driver { delivery } => delivery.sender.clone(),
            _ => {
                let delivery = self.outbox().context("delivery has no outbox payload")?;
                crate::pump::destination(&delivery.target, &delivery.msg)?
            }
        })
    }

    fn outbox(&self) -> Option<&OutboxDelivery> {
        match self {
            Self::Spawn { delivery }
            | Self::Call { delivery }
            | Self::Effect { delivery }
            | Self::Revoke { delivery }
            | Self::Promote { delivery }
            | Self::Stop { delivery }
            | Self::Shutdown { delivery }
            | Self::Link { delivery }
            | Self::Unlink { delivery }
            | Self::Monitor { delivery }
            | Self::Demonitor { delivery }
            | Self::Down { delivery }
            | Self::Exit { delivery }
            | Self::Subscription { delivery }
            | Self::Frame { delivery }
            | Self::Stream { delivery }
            | Self::Driver { delivery } => Some(delivery),
            _ => None,
        }
    }

    pub(crate) fn from_outbox(delivery: OutboxDelivery) -> Result<Self> {
        let kind = delivery.target.split(':').next().context("missing outbox kind")?;
        Ok(match kind {
            "spawn" => Self::Spawn { delivery },
            "call" => Self::Call { delivery },
            "effect" => Self::Effect { delivery },
            "revoke" => Self::Revoke { delivery },
            "promote" => Self::Promote { delivery },
            "stop" => Self::Stop { delivery },
            "shutdown" => Self::Shutdown { delivery },
            "link" => Self::Link { delivery },
            "unlink" => Self::Unlink { delivery },
            "monitor" => Self::Monitor { delivery },
            "demonitor" => Self::Demonitor { delivery },
            "down" => Self::Down { delivery },
            "exit" => Self::Exit { delivery },
            "sub" => Self::Subscription { delivery },
            "frame" => Self::Frame { delivery },
            "ws" => Self::Stream { delivery },
            "drv" => Self::Driver { delivery },
            _ => {
                crate::ids::check(&delivery.target)?;
                Self::Message { target: delivery.target, key: delivery.key, sender: delivery.sender, msg: delivery.msg }
            }
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ack {
    pub ok: bool,
    #[serde(default)]
    pub conflict: bool,
    pub owner: Option<String>,
    pub addr: Option<String>,
    pub error: Option<String>,
    pub result: Option<Value>,
}
impl Ack {
    pub(crate) fn success(result: Option<Value>) -> Self {
        Self { ok: true, conflict: false, owner: None, addr: None, error: None, result }
    }
    fn failure(error: anyhow::Error) -> Self {
        let conflict = error.downcast_ref::<crate::remote_store::LeaseLost>().is_some();
        Self { ok: false, conflict, owner: None, addr: None, error: Some(format!("{error:#}")), result: None }
    }
}

impl Node {
    /// Both ingress and local pump dispatch here; helpers own each actual write.
    pub async fn apply_delivery(&self, op: DeliveryOp) -> Result<bool> {
        let target = op.target()?;
        // Driver ids (drivers.rs) are node-local destinations, not actor ids.
        if !target.starts_with("drv:") {
            crate::ids::check(&target)?;
        }
        if let Some(delivery) = op.outbox() {
            let incarnation = crate::ids::incarnation(&delivery.sender, delivery.generation);
            let entry =
                crate::pump::Delivery { seq: delivery.seq, idx: delivery.idx, target: delivery.target.clone(), msg: delivery.msg.clone() };
            return Box::pin(self.deliver_outbox(&delivery.sender, delivery.generation, &incarnation, &entry, &delivery.key)).await;
        }
        match op {
            DeliveryOp::Message { target, key, sender, msg } => self.deliver_message(&target, &key, &sender, &msg).await?,
            DeliveryOp::Relationship { target, write } => return Box::pin(self.apply_relationship(&target, &write)).await,
            DeliveryOp::Publish { target } => {
                let receiver = self.open_actor(&target).await?;
                let mut conn = receiver.conn.lock().await;
                let tx = conn.transaction().await?;
                actor::set_meta(&tx, "ready", "true").await?;
                self.commit_control(&target, tx).await?;
                self.wake_actor(&target)?;
            }
            DeliveryOp::Release { target } => self.release_actor(&target).await?,
            DeliveryOp::Adopt { target } => {
                self.open_actor(&target).await?;
            }
            _ => anyhow::bail!("read/control delivery must use ingress result dispatch"),
        }
        Ok(true)
    }

    pub(crate) fn route_delivery(&self, op: DeliveryOp) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>> {
        Box::pin(async move {
            let target = op.target()?;
            match self.resolve(&target).await? {
                Placement::Remote { addr, .. } => {
                    if let DeliveryOp::Message { sender, .. } = &op {
                        let locally_open = self.connections.lock().await.contains_key(sender);
                        if locally_open {
                            self.ship_inner(sender).await?;
                            self.check_lease(sender)?;
                        }
                    }
                    let acks = self.forward(&addr, &[op]).await?;
                    ensure!(acks.len() == 1, "actor {target} seq -1: ingress returned wrong ack count");
                    let ack = &acks[0];
                    ensure!(ack.ok, "actor {target} seq -1: ingress: {}", ack.error.as_deref().unwrap_or("ownership changed"));
                    Ok(true)
                }
                Placement::Local | Placement::Unowned => Box::pin(self.apply_delivery(op)).await,
            }
        })
    }

    /// A held acknowledgement leaves through a covering publication, lease loss,
    /// close, or cancellation of this request. No detached ack state survives it.
    pub async fn apply_ingress(&self, ops: Vec<DeliveryOp>) -> Vec<Ack> {
        let mut acks = Vec::with_capacity(ops.len());
        for op in ops {
            let seq = op.outbox().map_or(-1, |delivery| delivery.seq);
            let result = match op.target() {
                Ok(target) => self.ingress_one(op).await.with_context(|| format!("actor {target} seq {seq}: apply ingress")),
                Err(error) => Err(error.context("actor <ingress> seq -1: decode delivery destination")),
            };
            let ack = result.unwrap_or_else(Ack::failure);
            let stop = !ack.ok;
            acks.push(ack);
            if stop {
                break;
            }
        }
        acks
    }

    async fn ingress_one(&self, op: DeliveryOp) -> Result<Ack> {
        let _admission = self.admit().await?;
        let target = op.target()?;
        if matches!(&op, DeliveryOp::Adopt { .. }) {
            // A release changes placement before a formerly cached lease expires.
            self.invalidate_placement(&target)?;
        }
        match self.resolve(&target).await? {
            Placement::Remote { node_id, addr } => {
                return Ok(Ack {
                    ok: false,
                    conflict: true,
                    owner: Some(node_id),
                    addr: Some(addr),
                    error: Some("placement changed".into()),
                    result: None,
                });
            }
            Placement::Unowned if matches!(&op, DeliveryOp::Release { .. }) => {
                return Ok(Ack {
                    ok: false,
                    conflict: true,
                    owner: None,
                    addr: None,
                    error: Some("placement changed".into()),
                    result: None,
                });
            }
            Placement::Local | Placement::Unowned => {}
        }
        let result = match op {
            DeliveryOp::Authority { cap, right, operation, .. } => {
                ensure!(target == cap.target, "actor {target} seq -1: authority target mismatch");
                Some(self.ingress_authority(&cap, right, &operation).await?)
            }
            DeliveryOp::Mint { rights, .. } => Some(self.ingress_mint(&target, rights).await?),
            DeliveryOp::Inspect { cap, query, params, .. } => {
                ensure!(target == cap.target, "actor {target} seq -1: inspection target mismatch");
                Some(self.ingress_inspect(&cap, query.as_deref(), params).await?)
            }
            DeliveryOp::State { .. } => Some(serde_json::to_value(Box::pin(self.child_state(&target)).await?)?),
            DeliveryOp::Children { .. } => Some(serde_json::to_value(Box::pin(self.child_ids(&target)).await?)?),
            DeliveryOp::Release { .. } => {
                self.release_actor(&target).await?;
                return Ok(Ack::success(None));
            }
            DeliveryOp::Adopt { .. } => {
                self.open_actor(&target).await?;
                self.ship_inner(&target).await?;
                Some(serde_json::json!({"epoch":self.lease_epoch(&target)?}))
            }
            DeliveryOp::HostSpawn { spec, .. } => {
                let child = self.spawn_inner(&target, &spec).await?;
                self.ship_inner(&child).await?;
                self.ship_inner(&target).await?;
                Some(serde_json::to_value(child)?)
            }
            op => {
                ensure!(Box::pin(self.apply_delivery(op)).await?, "actor {target} seq -1: delivery is pending");
                self.await_receiver_ship(&target).await?;
                None
            }
        };
        Ok(Ack::success(result))
    }
}
