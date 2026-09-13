//! Publication state is scoped to the actor connection lock.
use crate::{Node, actor, remote_store::Head, shipping_history::Segment};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashMap,
    sync::{Mutex, atomic::AtomicBool},
};
use turso::Connection;

pub(crate) struct AttemptControl<'a> {
    pub node: &'a Node,
    pub cancellation: &'a tokio::sync::Notify,
}
#[derive(Clone)]
pub(crate) struct Published {
    pub head: Head,
    pub changes: Option<u64>,
    pub allocated: i64,
    pub snapshot: bool,
    pub baseline: Option<std::sync::Arc<Segment>>,
}
#[derive(Default)]
pub(crate) struct ShippingState {
    pub actors: Mutex<HashMap<String, Published>>,
    pub failures: Mutex<Vec<crate::ShippingFailure>>,
    pub closed: AtomicBool,
    pub paused: AtomicBool,
}
impl ShippingState {
    pub fn get(&self, id: &str) -> Result<Published> {
        self.actors
            .lock()
            .map_err(|_| anyhow::anyhow!("shipping state poisoned"))?
            .get(id)
            .cloned()
            .with_context(|| format!("actor {id}: missing publication state"))
    }
    pub fn put(&self, id: &str, published: Published) -> Result<()> {
        self.actors.lock().map_err(|_| anyhow::anyhow!("shipping state poisoned"))?.insert(id.into(), published);
        Ok(())
    }
}
impl Node {
    pub(crate) async fn admit(&self) -> Result<tokio::sync::OwnedRwLockReadGuard<()>> {
        let guard = self.admission.clone().read_owned().await;
        ensure!(!self.shipping.closed.load(std::sync::atomic::Ordering::Acquire), "node is closed");
        Ok(guard)
    }

    pub(crate) fn check_lease(&self, id: &str) -> Result<()> {
        if let Some(store) = &self.remote {
            store.check(id)?;
        }
        Ok(())
    }
    pub(crate) async fn commit_control(&self, id: &str, tx: turso::transaction::Transaction<'_>) -> Result<()> {
        let prepared: Result<()> = async {
            self.check_lease(id)?;
            if self.remote.is_some() && actor::meta(&tx, "durability").await? == "remote" {
                self.ship_connection(id, &tx, false).await?;
            }
            self.check_lease(id)
        }
        .await;
        if let Err(error) = prepared {
            tx.rollback().await?;
            return Err(error);
        }
        tx.commit().await?;
        Ok(())
    }
    pub(crate) fn request_snapshot(&self, id: &str) -> Result<()> {
        if self.remote.is_some() {
            let mut published = self.shipping.get(id)?;
            published.snapshot = true;
            self.shipping.put(id, published)?;
        }
        Ok(())
    }
    pub(crate) async fn reset_durability(&self, id: &str, conn: &Connection) -> Result<()> {
        if self.remote.is_none() {
            return Ok(());
        }
        self.check_lease(id)?;
        if actor::meta(conn, "durability").await? == "remote" { self.checkpoint_remote(id, conn).await } else { self.request_snapshot(id) }
    }
    pub(crate) async fn checkpoint_remote(&self, id: &str, conn: &Connection) -> Result<()> {
        let store = self.remote.as_ref().context("checkpoint requires object store")?;
        self.check_lease(id)?;
        let previous = self.shipping.get(id)?;
        let seq = previous
            .allocated
            .max(previous.head.seq)
            .max(actor::meta(conn, "durability_seq").await?.parse::<i64>()?)
            .checked_add(1)
            .context("snapshot sequence overflow")?;
        let mut reserved = previous.clone();
        reserved.allocated = seq;
        self.shipping.put(id, reserved)?;
        actor::set_meta(conn, "durability_seq", &seq.to_string()).await?;
        let path = self.path(id).with_extension("shipping-snapshot");
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        conn.execute(format!("VACUUM INTO '{}'", path.to_str().context("snapshot path is not UTF-8")?.replace('\'', "''")), ()).await?;
        let key = format!("actors/{id}/snapshots/{}-{seq}.db", previous.head.epoch);
        let result = store.put(&key, std::fs::read(&path)?).await;
        std::fs::remove_file(&path)?;
        result?;
        let head = Head { epoch: previous.head.epoch, seq, snapshot_seq: seq, snapshot: Some(key), segments: Vec::new() };
        store.publish(id, head.clone()).await?;
        self.shipping.put(
            id,
            Published {
                head,
                allocated: seq,
                changes: Some(changes(conn).await?),
                snapshot: false,
                baseline: Some(std::sync::Arc::new(Segment::capture(conn, 0, seq).await?)),
            },
        )
    }
    pub(crate) async fn prepare_commit(&self, id: &str, conn: &Connection) -> Result<()> {
        let Some(store) = &self.remote else {
            return Ok(());
        };
        store.check(id)?;
        let published = self.shipping.get(id)?;
        let seq = actor::meta(conn, "durability_seq")
            .await?
            .parse::<i64>()?
            .max(published.allocated)
            .checked_add(1)
            .context("durability sequence overflow")?;
        actor::set_meta(conn, "durability_seq", &seq.to_string()).await?;
        if actor::meta(conn, "durability").await? == "remote" {
            self.ship_connection(id, conn, false).await?;
        }
        store.check(id)
    }

    /// Flush one actor and prove its cached head version is still current.
    pub async fn ship(&self, id: &str) -> Result<()> {
        let _admission = self.admit().await?;
        self.ship_inner(id).await
    }
    pub(crate) async fn ship_inner(&self, id: &str) -> Result<()> {
        if self.remote.is_none() {
            return Ok(());
        }
        let cached = self.connections.lock().await.get(id).cloned();
        let connection = match cached {
            Some(connection) => connection,
            None => self.open_actor(id).await?.conn,
        };
        let mut conn = connection.lock().await;
        // Do not hold the receiver lock while paused: ingress must be able to
        // commit a row whose acknowledgement then waits for this publication.
        if self.shipping.get(id)?.changes != Some(changes(&conn).await?) {
            drop(conn);
            self.wait_shipping_enabled(id).await?;
            conn = connection.lock().await;
        }
        let result = self.ship_connection(id, &conn, true).await;
        if let Err(error) = &result {
            self.record_shipping_failure(id, error);
            if error.downcast_ref::<crate::remote_store::LeaseLost>().is_some() {
                self.archive_stale(id, &mut conn).await?;
            }
        }
        result
    }

    pub(crate) async fn ship_connection(&self, id: &str, conn: &Connection, fence: bool) -> Result<()> {
        let Some(store) = &self.remote else {
            return Ok(());
        };
        let mut published = self.shipping.get(id)?;
        if published.snapshot || (conn.is_autocommit()? && published.head.seq - published.head.snapshot_seq >= self.config.snapshot_every) {
            return self.checkpoint_remote(id, conn).await;
        }
        let count = changes(conn).await?;
        if published.changes == Some(count) {
            if fence {
                store.fence(id, published.head).await?;
            }
            return Ok(());
        }
        ensure!(published.head.snapshot_seq >= 0, "actor {id}: initial remote snapshot is missing");
        let current: i64 = actor::meta(conn, "durability_seq").await?.parse()?;
        let seq = current.max(published.allocated.checked_add(1).context("head sequence overflow")?);
        published.allocated = published.allocated.max(seq);
        self.shipping.put(id, published.clone())?;
        if current != seq {
            actor::set_meta(conn, "durability_seq", &seq.to_string()).await?;
        }
        let full = std::sync::Arc::new(Segment::capture(conn, published.head.seq + 1, seq).await?);
        let segment = full.delta_from(published.baseline.as_ref().context("publication baseline missing")?)?.encode()?;
        let key = format!("actors/{id}/segments/{}-{}-{seq}.bin", published.head.epoch, published.head.seq + 1);
        // Fence first on explicit flush, including stale owners with expired clocks.
        // A takeover may race after this; the final head CAS remains authoritative.
        if fence {
            store.fence(id, published.head.clone()).await?;
        }
        store.put(&key, segment).await?;
        let mut head = published.head;
        head.seq = seq;
        head.segments.push(key);
        store.publish(id, head.clone()).await?;
        self.shipping
            .put(id, Published { allocated: seq, snapshot: false, head, changes: Some(changes(conn).await?), baseline: Some(full) })
    }

    pub(crate) async fn initialize_durability(&self, id: &str, conn: &Connection) -> Result<()> {
        for field in ["durability", "durability_seq"] {
            if actor::query(conn, "SELECT value FROM meta WHERE key=?", [field]).await?.rows.is_empty() {
                actor::set_meta(conn, field, if field == "durability" { "local" } else { "0" }).await?;
            }
        }
        if actor::code(conn).await?.hash == crate::supervisor::HASH
            && !actor::query(conn, "SELECT name FROM sqlite_schema WHERE type='table' AND name='spec'", ()).await?.rows.is_empty()
        {
            let columns = actor::query(conn, "PRAGMA table_info(spec)", ()).await?;
            if !columns.rows.iter().any(|row| row.get::<String>(1).is_ok_and(|name| name == "durability")) {
                conn.execute("ALTER TABLE spec ADD COLUMN durability TEXT NOT NULL DEFAULT 'local'", ()).await?;
            }
        }
        let Some(store) = &self.remote else {
            return Ok(());
        };
        let mut published = self.shipping.get(id)?;
        actor::set_meta(conn, "lease_epoch", &published.head.epoch.to_string()).await?;
        if published.head.snapshot_seq < 0 {
            actor::set_meta(conn, "durability_seq", "0").await?;
            let path = self.path(id).with_extension("shipping-snapshot");
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            conn.execute(format!("VACUUM INTO '{}'", path.to_str().context("snapshot path is not UTF-8")?.replace('\'', "''")), ()).await?;
            let key = format!("actors/{id}/snapshots/{}-0.db", published.head.epoch);
            let result = store.put(&key, std::fs::read(&path)?).await;
            std::fs::remove_file(&path)?;
            result?;
            published.head.snapshot_seq = 0;
            published.head.snapshot = Some(key);
            store.publish(id, published.head.clone()).await?;
            published.changes = Some(changes(conn).await?);
            self.shipping.put(id, published.clone())?;
        }
        if published.baseline.is_none() {
            published.baseline = Some(std::sync::Arc::new(Segment::capture(conn, 0, published.head.seq).await?));
            self.shipping.put(id, published)?;
        }
        Ok(())
    }
}

async fn changes(conn: &Connection) -> Result<u64> {
    let rows = actor::query(conn, "SELECT total_changes()", ()).await?;
    Ok(rows.rows.first().context("missing total_changes result")?.get::<i64>(0)?.try_into()?)
}
