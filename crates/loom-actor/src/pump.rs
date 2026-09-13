use crate::{EffectKey, Node, Spawn, Status, actor, ids};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeSet;

struct Delivery {
    seq: i64,
    idx: i64,
    target: String,
    msg: Vec<u8>,
}

impl Node {
    /// Deliver only committed outbox entries. Forks refuse delivery after any promotion.
    pub(crate) async fn pump_inner(&self, id: &str) -> Result<bool> {
        self.pump_unlocked(id).await
    }

    pub(crate) async fn deliver_message(&self, target: &str, key: &str, sender: &str, msg: &[u8]) -> Result<()> {
        if self.complete_call(target, sender, key, msg).await? {
            return Ok(());
        }
        let receiver = self.open_actor(target).await?;
        let mut conn = receiver.conn.lock().await;
        let tx = conn.transaction().await?;
        actor::inject(&tx, key, sender, msg).await?;
        self.commit_control(target, tx).await?;
        self.wake_actor(target)?;
        Ok(())
    }

    /// A failed destination blocks its own later rows; other pairs keep moving.
    pub(crate) async fn pump_unlocked(&self, id: &str) -> Result<bool> {
        let _sender = self.guard(&format!("pump:{id}")).await;
        let source = self.open_actor(id).await?;
        let generation: i64;
        let mut deliveries = Vec::new();
        {
            let conn = source.conn.lock().await;
            if actor::status(&conn).await? == Status::Fork
                || !actor::query(&conn, "SELECT value FROM meta WHERE key='replay_source'", ()).await?.rows.is_empty()
            {
                return Ok(false);
            }
            generation = actor::meta(&conn, "generation").await?.parse()?;
            let rows = actor::query(&conn, "SELECT seq,idx,target,msg FROM outbox WHERE delivered=0 ORDER BY seq,idx", ()).await?;
            for row in rows.rows {
                deliveries.push(Delivery { seq: row.get(0)?, idx: row.get(1)?, target: row.get(2)?, msg: row.get(3)? });
            }
        }
        let incarnation = ids::incarnation(id, generation);
        let mut blocked = BTreeSet::new();
        let mut progressed = false;
        let mut failure = None;
        for entry in deliveries {
            let destination = destination(&entry.target, &entry.msg)?;
            if blocked.contains(&destination) {
                continue;
            }
            let delivery_key = format!("{incarnation}:{}:{}", entry.seq, entry.idx);
            self.check_lease(id)?;
            let result = self
                .deliver_outbox(id, generation, &incarnation, &entry, &delivery_key)
                .await
                .with_context(|| format!("actor {id} seq {}: deliver outbox {}", entry.seq, entry.idx));
            match result {
                Ok(true) => {
                    let mut conn = source.conn.lock().await;
                    if actor::meta(&conn, "generation").await?.parse::<i64>()? != generation {
                        progressed = true;
                        break;
                    }
                    self.check_lease(id)?;
                    let tx = conn.transaction().await?;
                    if entry.target == "spawn" && matches!(serde_json::from_slice::<Spawn>(&entry.msg)?, Spawn::Restart { .. }) {
                        actor::set_meta(&tx, &format!("publish:{destination}"), "1").await?;
                    }
                    tx.execute("UPDATE outbox SET delivered=1 WHERE seq=? AND idx=?", turso::params![entry.seq, entry.idx]).await?;
                    self.commit_control(id, tx).await?;
                    progressed = true;
                }
                Ok(false) => {
                    blocked.insert(destination);
                }
                Err(error) => {
                    blocked.insert(destination);
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
        }
        let publications = actor::query(&*source.conn.lock().await, "SELECT key FROM meta WHERE key LIKE 'publish:%'", ()).await?;
        for row in publications.rows {
            let marker: String = row.get(0)?;
            let child = marker.strip_prefix("publish:").context("invalid publication marker")?;
            if blocked.contains(child) {
                continue;
            }
            {
                let published = self.open_actor(child).await?;
                let mut conn = published.conn.lock().await;
                let tx = conn.transaction().await?;
                actor::set_meta(&tx, "ready", "true").await?;
                self.commit_control(child, tx).await?;
                self.wake_actor(child)?;
            }
            let mut conn = source.conn.lock().await;
            let tx = conn.transaction().await?;
            tx.execute("DELETE FROM meta WHERE key=?", [marker]).await?;
            self.commit_control(id, tx).await?;
        }
        progressed |= self.sync_shutdown_requests(id).await?;
        let sync_result = self.sync_index(id).await;
        if let Some(error) = failure {
            return Err(error);
        }
        sync_result?;
        Ok(progressed)
    }

    /// Restart groups wait for every requested sibling shutdown to finish.
    async fn children_shutting_down(&self, parent: &str) -> Result<bool> {
        let source = self.open_actor(parent).await?;
        Ok(!actor::query(&*source.conn.lock().await, "SELECT child FROM shutdowns LIMIT 1", ()).await?.rows.is_empty())
    }

    async fn deliver_outbox(&self, id: &str, generation: i64, incarnation: &str, entry: &Delivery, key: &str) -> Result<bool> {
        if entry.target == "spawn" {
            match serde_json::from_slice::<Spawn>(&entry.msg)? {
                Spawn::Child { id: child, spec, origin_seq, origin_idx } => {
                    ensure!(child == ids::child(incarnation, origin_seq, origin_idx), "spawn id mismatch");
                    self.create(&child, id, &spec.behavior_hash, &spec.init, spec.durability).await?;
                    if spec.link {
                        self.relate(id, &child, "link", &[], key).await?;
                    }
                    if spec.monitor {
                        self.monitor(id, &child, &format!("spawn:{child}")).await?;
                    }
                    let published = self.open_actor(&child).await?;
                    let mut conn = published.conn.lock().await;
                    let tx = conn.transaction().await?;
                    actor::set_meta(&tx, "durability", spec.durability.name()).await?;
                    actor::set_meta(&tx, "shutdown", &serde_json::to_string(&spec.shutdown)?).await?;
                    actor::set_meta(&tx, "ready", "true").await?;
                    self.commit_control(&child, tx).await?;
                    drop(conn);
                    self.wake_actor(&child)?;
                    self.sync_index(&child).await?;
                }
                Spawn::Restart { id: child, verb } => {
                    if self.children_shutting_down(id).await? {
                        return Ok(false);
                    }
                    if !self.restart_unlocked(&child, verb, key).await? {
                        return Ok(false);
                    }
                    self.wake_actor(&child)?;
                    self.sync_index(&child).await?;
                }
            }
        } else if let Some(target) = entry.target.strip_prefix("call:") {
            self.deliver_call(id, target, &entry.msg, key).await?;
        } else if let Some(kind) = entry.target.strip_prefix("effect:") {
            if kind == "alarm" && self.arm_alarm(id, &entry.msg).await? {
                return Ok(true);
            }
            let effect_key = EffectKey { actor_id: id.into(), seq: entry.seq, idx: entry.idx, generation };
            let result = self.effects.call(&effect_key, kind, &entry.msg).await?;
            self.deliver_message(id, &format!("req:{}:{}", entry.seq, entry.idx), id, &result).await?;
        } else if entry.target.contains(':') {
            let mut parts = entry.target.splitn(2, ':');
            let kind = parts.next().context("missing outbox kind")?;
            let target = parts.next().context("missing outbox destination")?;
            self.relate(id, target, kind, &entry.msg, key).await?;
        } else {
            self.deliver_message(&entry.target, key, id, &entry.msg).await?;
        }
        Ok(true)
    }
}

fn destination(target: &str, msg: &[u8]) -> Result<String> {
    if target == "spawn" {
        return Ok(match serde_json::from_slice::<Spawn>(msg)? {
            Spawn::Child { id, .. } | Spawn::Restart { id, .. } => id,
        });
    }
    if target.starts_with("effect:") {
        return Ok(target.to_owned());
    }
    if target.starts_with("demonitor:") {
        return Ok(target.to_owned());
    }
    let mut parts = target.splitn(2, ':');
    let first = parts.next().context("missing target")?;
    Ok(parts.next().unwrap_or(first).to_owned())
}
