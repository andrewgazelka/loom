//! Restore precedes connection publication; lease loss preserves the losing file.
use crate::{Node, actor, durability::Published};
use anyhow::{Context, Result};
use turso::Connection;

impl Node {
    pub(crate) async fn restore_on_open(&self, id: &str) -> Result<()> {
        if self.remote.is_none() {
            return Ok(());
        }
        if self.shipping.get(id).is_ok() {
            match self.check_lease(id) {
                Ok(()) => return Ok(()),
                Err(error) if error.downcast_ref::<crate::remote_store::LeaseLost>().is_some() => {}
                Err(error) => return Err(error),
            }
            self.shipping.actors.lock().map_err(|_| anyhow::anyhow!("shipping state poisoned"))?.remove(id);
            if self.path(id).exists() {
                self.archive_unowned_file(id).await?;
            }
        }
        let result = self.restore_new_owner(id).await;
        if result.is_err() {
            self.shipping.actors.lock().map_err(|_| anyhow::anyhow!("shipping state poisoned"))?.remove(id);
        }
        result
    }
    async fn restore_new_owner(&self, id: &str) -> Result<()> {
        let store = self.remote.as_ref().context("restore requires object store")?;
        let head = store.acquire(id).await?;
        self.shipping.put(id, Published { allocated: head.seq, head: head.clone(), snapshot: false, changes: None, baseline: None })?;
        let path = self.path(id);
        let mut restore = !path.exists();
        if path.exists() && head.snapshot_seq >= 0 {
            let conn = actor::connect(&path, self.config.io).await?;
            let local = actor::query(&conn, "SELECT value FROM meta WHERE key='durability_seq'", ()).await?;
            let seq = local.rows.first().map(|r| r.get::<String>(0)).transpose()?.map(|s| s.parse::<i64>()).transpose()?.unwrap_or(0);
            let epoch = actor::query(&conn, "SELECT value FROM meta WHERE key='lease_epoch'", ()).await?;
            let epoch = epoch.rows.first().map(|r| r.get::<String>(0)).transpose()?.map(|s| s.parse::<u64>()).transpose()?.unwrap_or(0);
            restore = seq < head.seq || epoch.checked_add(1) != Some(head.epoch);
            if !restore {
                self.initialize_durability(id, &conn).await?;
                self.checkpoint_remote(id, &conn).await?;
            }
            drop(conn);
            if restore {
                let archive = self.dir.join(format!("{id}.stale.{epoch}.db"));
                anyhow::ensure!(!archive.exists(), "actor {id}: stale archive already exists at {}", archive.display());
                std::fs::rename(&path, &archive)?;
                move_wal(&path, &archive)?;
            }
        }
        if restore && head.snapshot_seq >= 0 {
            let result: Result<()> = async {
                let key = head.snapshot.as_ref().context(format!("actor {id}: head is missing snapshot key"))?;
                anyhow::ensure!(key.starts_with(&format!("actors/{id}/snapshots/")), "actor {id}: foreign snapshot key {key}");
                let snapshot = store.get(key).await?;
                let mut segments = Vec::new();
                anyhow::ensure!(head.snapshot_seq >= 0 && head.seq >= head.snapshot_seq, "actor {id}: invalid head revision");
                let mut expected = head.snapshot_seq.checked_add(1).context("snapshot revision overflow")?;
                for key in &head.segments {
                    anyhow::ensure!(
                        key.starts_with(&format!("actors/{id}/segments/")),
                        "actor {id}: head references foreign segment {key}"
                    );
                    let bytes = store.get(key).await?;
                    let segment = crate::shipping_history::Segment::decode(&bytes)?;
                    anyhow::ensure!(
                        segment.from_seq == expected && segment.to_seq <= head.seq,
                        "actor {id}: head history range mismatch: expected from_seq {expected}, segment from_seq {} to_seq {}, head.seq {}",
                        segment.from_seq,
                        segment.to_seq,
                        head.seq
                    );
                    expected = segment.to_seq.checked_add(1).context("segment revision overflow")?;
                    segments.push(bytes);
                }
                anyhow::ensure!(
                    expected == head.seq.checked_add(1).context("head revision overflow")?,
                    "actor {id}: incomplete head history: expected from_seq {expected}, head.seq {}",
                    head.seq
                );
                self.restore_history(&snapshot, &segments, id, &path, head.snapshot_seq, head.seq).await?;
                let conn = actor::connect(&path, self.config.io).await?;
                actor::set_meta(&conn, "durability_seq", &head.seq.to_string()).await?;
                Ok(())
            }
            .await;
            if result.is_err() {
                self.shipping.actors.lock().map_err(|_| anyhow::anyhow!("shipping state poisoned"))?.remove(id);
            }
            result?;
        }
        Ok(())
    }

    /// Stale files are preserved by design; never listed; removed only by an operator.
    pub(crate) async fn archive_stale(&self, id: &str, conn: &mut Connection) -> Result<()> {
        let epoch: u64 = actor::meta(conn, "lease_epoch").await?.parse()?;
        let archive = self.dir.join(format!("{id}.stale.{epoch}.db"));
        if archive.exists() && actor::status(conn).await? == crate::Status::Stopped && actor::meta(conn, "reason").await? == "lease_lost" {
            return Ok(());
        }
        actor::set_meta(conn, "status", "stopped").await?;
        actor::set_meta(conn, "reason", "lease_lost").await?;
        anyhow::ensure!(!archive.exists(), "actor {id}: stale archive already exists at {}", archive.display());
        let old = std::mem::replace(conn, actor::connect(std::path::Path::new(":memory:"), crate::Io::Memory).await?);
        drop(old);
        std::fs::rename(self.path(id), &archive)?;
        move_wal(&self.path(id), &archive)?;
        *conn = actor::connect(&archive, self.config.io).await?;
        self.connections.lock().await.remove(id);
        self.shipping.actors.lock().map_err(|_| anyhow::anyhow!("shipping state poisoned"))?.remove(id);
        self.invalidate_placement(id)?;
        eprintln!("actor {id}: lease_lost; preserved {}", archive.display());
        Ok(())
    }

    /// Startup only: no connection has been admitted for this losing local file.
    pub(crate) async fn archive_unowned_file(&self, id: &str) -> Result<()> {
        let mut conn = actor::connect(&self.path(id), self.config.io).await?;
        self.archive_stale(id, &mut conn).await
    }
}
fn move_wal(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    for suffix in ["-wal", "-shm"] {
        let source = std::path::PathBuf::from(format!("{}{suffix}", from.display()));
        if source.exists() {
            std::fs::rename(source, format!("{}{suffix}", to.display()))?;
        }
    }
    Ok(())
}
