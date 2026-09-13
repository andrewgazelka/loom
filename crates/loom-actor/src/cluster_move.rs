//! Explicit movement reuses the lease release and stale-file lifecycle.
use crate::{DeliveryOp, Node, Placement};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct MoveResult { pub node_id: String, pub epoch: u64 }

impl Node {
    pub(crate) async fn release_actor(&self, id: &str) -> Result<()> {
        let _move = self.guard(&format!("open:{id}")).await;
        let store = self.remote.as_ref().context("--store is required for move")?;
        let connection = self.connections.lock().await.get(id).cloned();
        if let Some(connection) = connection {
            let mut conn = connection.lock().await;
            self.check_lease(id)?;
            self.ship_connection(id, &conn, true).await?;
            // Stale files leave only through explicit operator removal. A failed
            // lease release leaves this archive and is retried below on the next move.
            self.archive_stale(id, &mut conn).await?;
        } else {
            let epoch = store.epoch(id)?;
            ensure!(self.dir.join(format!("{id}.stale.{epoch}.db")).exists(), "actor {id} seq -1: release has no owned file");
        }
        store.release(id).await?;
        self.invalidate_placement(id)
    }

    pub async fn move_actor(&self, id: &str, node_id: &str) -> Result<MoveResult> {
        let _admission = self.admit().await?;
        let identity = self.identity().context("--store and --cluster-key-file are required for move")?;
        let target = self.nodes().await?.into_iter().find(|node| node.node_id == node_id).context("move target node is unknown")?;
        let placement = self.resolve(id).await?;
        let same_owner = match &placement {
            Placement::Local => identity.node_id == node_id,
            Placement::Remote { node_id: owner, .. } => owner == node_id,
            Placement::Unowned => true,
        };
        if !same_owner {
            match placement {
                Placement::Local => self.release_actor(id).await?,
                Placement::Remote { addr, .. } => {
                    let acks = self.forward(&addr, &[DeliveryOp::Release { target: id.into() }]).await?;
                    ensure!(acks.first().is_some_and(|ack| ack.ok), "actor {id} seq -1: move release refused");
                }
                Placement::Unowned => {}
            }
        }
        self.invalidate_placement(id)?;
        if identity.node_id == node_id {
            self.open_actor(id).await?;
            self.ship_inner(id).await?;
            return Ok(MoveResult { node_id: node_id.into(), epoch: self.lease_epoch(id)? });
        }
        let acks = self.forward(&target.addr, &[DeliveryOp::Adopt { target: id.into() }]).await?;
        ensure!(acks.first().is_some_and(|ack| ack.ok), "actor {id} seq -1: move adopt refused");
        let epoch = acks.first().and_then(|ack| ack.result.as_ref()).and_then(|value| value["epoch"].as_u64())
            .context("move adopt response missing epoch")?;
        Ok(MoveResult { node_id: node_id.into(), epoch })
    }
}
