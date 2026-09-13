use crate::{EffectKey, Node, Spawn, Status, actor, ids};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct Delivery {
    pub(crate) seq: i64,
    pub(crate) idx: i64,
    pub(crate) target: String,
    pub(crate) msg: Vec<u8>,
}

struct PairResult {
    destination: String,
    progressed: bool,
    blocked: bool,
    failure: Option<anyhow::Error>,
}

enum DeliveryProgress {
    Delivered,
    Pending,
    GenerationChanged,
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
        self.wake.notify_one();
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
        let mut pairs: BTreeMap<String, Vec<Delivery>> = BTreeMap::new();
        for entry in deliveries {
            pairs.entry(destination(&entry.target, &entry.msg)?).or_default().push(entry);
        }
        // join_next removes completed pair jobs; dropping this JoinSet aborts all
        // remaining jobs, whose unacknowledged rows remain pending for the next pump.
        let mut jobs = tokio::task::JoinSet::new();
        for (destination, entries) in pairs {
            let node = self.clone();
            let sender = id.to_owned();
            jobs.spawn(async move { node.pump_pair(&sender, generation, destination, entries).await });
        }
        let mut blocked = BTreeSet::new();
        let mut progressed = false;
        let mut failure = None;
        while let Some(result) = jobs.join_next().await {
            match result {
                Ok(pair) => {
                    progressed |= pair.progressed;
                    if pair.blocked {
                        blocked.insert(pair.destination);
                    }
                    if failure.is_none() {
                        failure = pair.failure;
                    }
                }
                Err(error) => return Err(error).with_context(|| format!("actor {id} seq -1: destination pump task")),
            }
        }
        let publications = actor::query(&*source.conn.lock().await, "SELECT key FROM meta WHERE key LIKE 'publish:%'", ()).await?;
        for row in publications.rows {
            let marker: String = row.get(0)?;
            let child = marker.strip_prefix("publish:").context("invalid publication marker")?;
            if blocked.contains(child) {
                continue;
            }
            self.route_outbox(id, -1, crate::DeliveryOp::Publish { target: child.into() }).await?;
            let mut conn = source.conn.lock().await;
            let tx = conn.transaction().await?;
            tx.execute("DELETE FROM meta WHERE key=?", [marker]).await?;
            self.commit_control(id, tx).await?;
            self.wake.notify_one();
        }
        progressed |= self.sync_shutdown_requests(id).await?;
        let sync_result = self.sync_index(id).await;
        if let Some(error) = failure {
            return Err(error);
        }
        sync_result?;
        Ok(progressed)
    }

    fn pump_pair<'a>(
        &'a self,
        id: &'a str,
        generation: i64,
        destination: String,
        entries: Vec<Delivery>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PairResult> + Send + 'a>> {
        Box::pin(async move {
            let mut result = PairResult { destination, progressed: false, blocked: false, failure: None };
            for entry in entries {
                match self.pump_pair_entry(id, generation, &result.destination, &entry).await {
                    Ok(DeliveryProgress::Delivered) => result.progressed = true,
                    Ok(DeliveryProgress::GenerationChanged) => {
                        result.progressed = true;
                        result.blocked = true;
                        break;
                    }
                    Ok(DeliveryProgress::Pending) => {
                        result.blocked = true;
                        break;
                    }
                    Err(error) => {
                        result.blocked = true;
                        result.failure = Some(error.context(format!("actor {id} seq {}: deliver outbox {}", entry.seq, entry.idx)));
                        break;
                    }
                }
            }
            result
        })
    }

    async fn pump_pair_entry(&self, id: &str, generation: i64, destination: &str, entry: &Delivery) -> Result<DeliveryProgress> {
        let source = self.open_actor(id).await?;
        {
            let conn = source.conn.lock().await;
            if actor::meta(&conn, "generation").await?.parse::<i64>()? != generation {
                return Ok(DeliveryProgress::GenerationChanged);
            }
        }
        self.check_lease(id)?;
        let incarnation = ids::incarnation(id, generation);
        let op = crate::DeliveryOp::from_outbox(crate::OutboxDelivery {
            sender: id.into(), generation, seq: entry.seq, idx: entry.idx,
            target: entry.target.clone(), key: format!("{incarnation}:{}:{}", entry.seq, entry.idx), msg: entry.msg.clone(),
        })?;
        if !self.route_outbox(id, entry.seq, op).await? {
            return Ok(DeliveryProgress::Pending);
        }
        let mut conn = source.conn.lock().await;
        if actor::meta(&conn, "generation").await?.parse::<i64>()? != generation {
            return Ok(DeliveryProgress::GenerationChanged);
        }
        self.check_lease(id)?;
        let tx = conn.transaction().await?;
        if entry.target == "spawn" && matches!(serde_json::from_slice::<Spawn>(&entry.msg)?, Spawn::Restart { .. }) {
            actor::set_meta(&tx, &format!("publish:{destination}"), "1").await?;
        }
        tx.execute("UPDATE outbox SET delivered=1 WHERE seq=? AND idx=?", turso::params![entry.seq, entry.idx]).await?;
        self.commit_control(id, tx).await?;
        Ok(DeliveryProgress::Delivered)
    }

    /// Restart groups wait for every requested sibling shutdown to finish.
    pub(crate) async fn children_shutting_down(&self, parent: &str) -> Result<bool> {
        let source = self.open_actor(parent).await?;
        Ok(!actor::query(
            &*source.conn.lock().await,
            "SELECT child FROM shutdowns UNION ALL SELECT target FROM outbox WHERE delivered=0 AND target LIKE 'shutdown:%' LIMIT 1",
            (),
        )
        .await?
        .rows
        .is_empty())
    }

    pub(crate) async fn deliver_outbox(&self, id: &str, generation: i64, incarnation: &str, entry: &Delivery, key: &str) -> Result<bool> {
        if entry.target == "spawn" {
            match serde_json::from_slice::<Spawn>(&entry.msg)? {
                Spawn::Child { id: child, spec, origin_seq, origin_idx } => {
                    ensure!(child == ids::child(incarnation, origin_seq, origin_idx), "spawn id mismatch");
                    self.create(&child, id, &spec.behavior_hash, &spec.init, spec.durability).await?;
                    if spec.link {
                        let link = crate::DeliveryOp::Link { delivery: crate::OutboxDelivery {
                            sender: id.into(), generation, seq: entry.seq, idx: entry.idx,
                            target: format!("link:{child}"), key: key.into(), msg: Vec::new(),
                        } };
                        self.route_outbox(&child, entry.seq, link).await?;
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
                    self.sync_index(&child).await?;
                }
                Spawn::Restart { id: child, verb } => {
                    if id.is_empty() {
                        Box::pin(self.pump_unlocked(&child)).await?;
                    }
                    if !self.restart_unlocked(&child, verb, key).await? {
                        return Ok(false);
                    }
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

pub(crate) fn destination(target: &str, msg: &[u8]) -> Result<String> {
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
