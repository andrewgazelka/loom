//! Relationship state belongs to its actor file; every second-file write resolves its owner.
use crate::{Node, Status, actor};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RelationshipWrite {
    Link { peer: String, key: String, linked: bool },
    Monitor { target: String, reference: String },
    RegisterMonitor { watcher: String, reference: String },
    Demonitor { reference: String, target: String },
    RemoveMonitor { reference: String },
    FlushDown { reference: String },
    Call { target: String, msg: Vec<u8>, key: String },
    ShutdownRequester { child: String, request: String },
}

impl Node {
    pub(crate) fn route_relationship<'a>(
        &'a self,
        target: &'a str,
        write: RelationshipWrite,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + 'a>> {
        Box::pin(async move {
            self.route_delivery(crate::DeliveryOp::Relationship { target: target.into(), write })
                .await
                .with_context(|| format!("actor {target} seq -1: route relationship"))
        })
    }

    // A boxed return type, not an `async fn`: this call sits on the delivery recursion cycle
    // (route_outbox -> apply_delivery -> relate -> relationship_from), and rustc cannot prove
    // `Send` for an opaque future that awaits itself through another opaque future.
    pub(crate) fn relationship_from<'a>(
        &'a self,
        source: &'a str,
        target: &'a str,
        write: RelationshipWrite,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + 'a>> {
        Box::pin(async move {
            self.route_outbox(source, -1, crate::DeliveryOp::Relationship { target: target.into(), write })
                .await
                .with_context(|| format!("actor {source} seq -1: route relationship to {target}"))
        })
    }

    pub(crate) async fn apply_relationship(&self, target: &str, write: &RelationshipWrite) -> Result<bool> {
        match write {
            RelationshipWrite::Monitor { target: watched, reference } => {
                self.monitor_local(target, watched, reference).await?;
                return Ok(true);
            }
            RelationshipWrite::Demonitor { reference, target: watched } => {
                self.demonitor_local(target, reference, watched).await?;
                return Ok(true);
            }
            RelationshipWrite::Call { target: callee, msg, key } => {
                self.deliver_call_local(target, callee, msg, key).await?;
                return Ok(true);
            }
            _ => {}
        }
        let owner = self.open_actor(target).await?;
        let mut conn = owner.conn.lock().await;
        let tx = conn.transaction().await?;
        match write {
            RelationshipWrite::Link { peer, key, linked } => {
                if !crate::supervision::applied(&tx, key).await? {
                    if *linked {
                        tx.execute("INSERT OR IGNORE INTO links(peer) VALUES (?)", [peer.as_str()]).await?;
                    } else {
                        tx.execute("DELETE FROM links WHERE peer=?", [peer.as_str()]).await?;
                    }
                    actor::set_meta(&tx, &format!("applied:{key}"), "1").await?;
                }
            }
            RelationshipWrite::RegisterMonitor { watcher, reference } => {
                // RemoveMonitor leaves this registration; its tombstone prevents a late retry resurrecting it.
                let done = format!("monitor_removed:{reference}");
                if !crate::supervision::applied(&tx, &done).await? {
                    let inserted = tx
                        .execute("INSERT OR IGNORE INTO monitored_by(ref,watcher) VALUES (?,?)", [reference.as_str(), watcher.as_str()])
                        .await?;
                    if inserted != 0 && actor::status(&tx).await? == Status::Stopped {
                        let generation = actor::meta(&tx, "generation").await?;
                        let counter = actor::meta(&tx, "event_counter").await?;
                        let reason = actor::meta(&tx, "reason").await?;
                        let msg = serde_json::json!({"type":"down","ref":reference,"from":target,"reason":reason,
                            "generation":generation.parse::<i64>()?,"event":format!("{target}:{generation}:{counter}")});
                        actor::enqueue(&tx, actor::cursor(&tx).await?, &format!("down:{watcher}"), &serde_json::to_vec(&msg)?).await?;
                    }
                }
            }
            RelationshipWrite::RemoveMonitor { reference } => {
                tx.execute("DELETE FROM monitored_by WHERE ref=?", [reference.as_str()]).await?;
                // Immutable incarnation-scoped receipt is retained with the actor history, like applied outbox keys.
                actor::set_meta(&tx, &format!("applied:monitor_removed:{reference}"), "1").await?;
            }
            RelationshipWrite::FlushDown { reference } => {
                tx.execute("DELETE FROM inbox WHERE key=? AND state!='done'", [format!("down:{reference}")]).await?;
                crate::mailbox::refresh_cursor(&tx).await?;
            }
            RelationshipWrite::ShutdownRequester { child, request } => {
                // sync_shutdown_requests removes the barrier after the child's owner reports Stopped.
                tx.execute(
                    "INSERT INTO shutdowns(child,request) VALUES (?,?) ON CONFLICT(child) DO UPDATE SET request=excluded.request",
                    [child.as_str(), request.as_str()],
                )
                .await?;
            }
            RelationshipWrite::Monitor { .. } | RelationshipWrite::Demonitor { .. } | RelationshipWrite::Call { .. } => {
                anyhow::bail!("actor {target} seq -1: relationship orchestration reached a file write")
            }
        }
        self.commit_control(target, tx).await?;
        self.wake.notify_one();
        Ok(true)
    }
}
