//! Publication coverage is connection-local total_changes, not inbox cursor.
use crate::{Node, actor};
use anyhow::{Context, Result, ensure};
use std::sync::atomic::Ordering;

impl Node {
    /// Pause explicit/background Local shipment. Remote transaction publication
    /// still runs. resume_shipping is the leaver of this deterministic test gate.
    pub fn pause_shipping(&self) { self.shipping.paused.store(true, Ordering::Release); }
    pub fn resume_shipping(&self) { self.shipping.paused.store(false, Ordering::Release); }

    pub(crate) async fn wait_shipping_enabled(&self, id: &str) -> Result<()> {
        while self.shipping.paused.load(Ordering::Acquire) {
            self.check_lease(id)?;
            ensure!(!self.shipping.closed.load(Ordering::Acquire), "actor {id} seq -1: node closed during shipping gate");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        Ok(())
    }

    pub(crate) async fn await_receiver_ship(&self, id: &str) -> Result<()> {
        if self.remote.is_none() { return Ok(()); }
        let receiver = self.open_actor(id).await?;
        let required = {
            let conn = receiver.conn.lock().await;
            self.check_lease(id)?;
            let rows = actor::query(&conn, "SELECT total_changes()", ()).await?;
            u64::try_from(rows.rows.first().context("missing total_changes result")?.get::<i64>(0)?)?
        };
        loop {
            self.check_lease(id)?;
            ensure!(!self.shipping.closed.load(Ordering::Acquire), "actor {id} seq -1: closed before ingress publication");
            if self.shipping.get(id)?.changes.is_some_and(|published| published >= required) { return Ok(()); }
            // All arrivals before the same ship tick observe the same publication.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
}
