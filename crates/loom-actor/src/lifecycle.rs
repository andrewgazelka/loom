use crate::supervision::applied;
use crate::{Node, RestartVerb, Shutdown, Status, actor};
use anyhow::{Context, Result, ensure};

struct CompletedShutdown {
    child: String,
    request: String,
}

/// A completed sync consumes its dirty bit. Failure or cancellation restores it.
struct ShutdownSync {
    node: Node,
    requester: String,
    complete: bool,
}
impl Drop for ShutdownSync {
    fn drop(&mut self) {
        if !self.complete {
            let mut state = self.node.scheduling.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.shutdown_dirty.insert(self.requester.clone());
            state.woken.insert(self.requester.clone());
            self.node.wake.notify_one();
        }
    }
}

impl Node {
    pub async fn stop(&self, id: &str, reason: &str) -> Result<()> {
        let _admission = self.admit().await?;
        self.stop_unlocked(id, reason, &format!("host:{}", ulid::Ulid::new()), "")
            .await
            .with_context(|| format!("actor {id} seq -1: stop"))?;
        self.wake_actor(id)?;
        Ok(())
    }

    pub(crate) async fn stop_unlocked(&self, id: &str, reason: &str, key: &str, initiator: &str) -> Result<()> {
        if id.starts_with("drv:") {
            return self.close_drivers(None, Some(crate::drivers::target(id)?.id)).await;
        }
        let _lifecycle = self.guard(&format!("lifecycle:{id}")).await;
        if reason == "kill"
            && let Some(task) = self.tasks.lock().await.get(id)
        {
            task.notify_one();
        }
        let actor = self.open_actor(id).await?;
        let mut conn = actor.conn.lock().await;
        let behavior = actor::behavior(&self.registry, &actor::code(&conn).await?.hash).await?;
        crate::hooks::stop(
            &mut conn,
            id,
            crate::hooks::Stop { reason, key, initiator },
            behavior.as_ref(),
            &crate::effects::RuntimeEffects { node: self, external: self.effects.as_ref() },
            self,
        )
        .await?;
        if actor::status(&conn).await? == Status::Stopped {
            self.shutdown_deadlines.lock().await.retain(|_, timer| timer.target != id);
        }
        drop(conn);
        self.sync_shutdowns(id).await
    }

    pub(crate) async fn kill_from_timer(&self, id: &str, initiator: &str, key: &str) -> Result<bool> {
        let _lifecycle = self.guard(&format!("lifecycle:{id}")).await;
        // Stop/reset remove cached deadlines under the connection lock before
        // releasing lifecycle admission. A stale scanner cannot cancel a new turn.
        if !self.shutdown_deadlines.lock().await.contains_key(key) {
            return Ok(false);
        }
        if let Some(task) = self.tasks.lock().await.get(id) {
            task.notify_one();
        }
        let target = self.open_actor(id).await?;
        let mut conn = target.conn.lock().await;
        let exists = !actor::query(&conn, "SELECT ref FROM timers WHERE ref=? AND kind='shutdown'", [key]).await?.rows.is_empty();
        if !exists {
            self.shutdown_deadlines.lock().await.remove(key);
            return Ok(false);
        }
        let behavior = actor::behavior(&self.registry, &actor::code(&conn).await?.hash).await?;
        crate::hooks::stop(
            &mut conn,
            id,
            crate::hooks::Stop { reason: "kill", key: &format!("{key}:kill"), initiator },
            behavior.as_ref(),
            &crate::effects::RuntimeEffects { node: self, external: self.effects.as_ref() },
            self,
        )
        .await?;
        self.shutdown_deadlines.lock().await.retain(|_, timer| timer.target != id);
        drop(conn);
        self.sync_shutdowns(id).await?;
        Ok(true)
    }

    /// A completed target wakes requester pumps to release their own barriers.
    pub(crate) async fn sync_shutdowns(&self, id: &str) -> Result<()> {
        self.scheduling()?.index_dirty.insert(id.into());
        if self.scheduling()?.timer_deadlines.contains_key(id) {
            self.request_timer_scan()?;
        }
        let requesters = self.scheduling()?.shutdown_requesters.get(id).cloned().unwrap_or_default();
        for requester in requesters {
            self.scheduling()?.shutdown_dirty.insert(requester.clone());
            self.wake_actor(&requester)?;
        }
        self.wake_actor(id)?;
        Ok(())
    }

    /// The requester's pump removes its committed rows after target completion.
    pub(crate) async fn sync_shutdown_requests(&self, requester: &str) -> Result<bool> {
        if !self.scheduling()?.shutdown_dirty.remove(requester) {
            return Ok(false);
        }
        let mut sync = ShutdownSync { node: self.clone(), requester: requester.into(), complete: false };
        let result = self.sync_shutdown_requests_inner(requester).await;
        sync.complete = result.is_ok();
        result
    }

    async fn sync_shutdown_requests_inner(&self, requester: &str) -> Result<bool> {
        let source = self.open_actor(requester).await?;
        let pending = actor::query(&*source.conn.lock().await, "SELECT child,request FROM shutdowns", ()).await?;
        let mut completed = Vec::new();
        for row in pending.rows {
            let child: String = row.get(0)?;
            let request: String = row.get(1)?;
            let target = self.capability_reader(&child).await?;
            if actor::status(&target).await? == Status::Stopped {
                completed.push(CompletedShutdown { child, request });
            }
        }
        if completed.is_empty() {
            return Ok(false);
        }
        let mut conn = source.conn.lock().await;
        let tx = conn.transaction().await?;
        let mut removed = Vec::new();
        for completion in completed {
            if tx
                .execute("DELETE FROM shutdowns WHERE child=? AND request=?", [completion.child.as_str(), completion.request.as_str()])
                .await?
                != 0
            {
                removed.push(completion);
            }
        }
        self.commit_control(requester, tx).await?;
        let mut state = self.scheduling()?;
        for completion in removed {
            if let Some(owners) = state.shutdown_requesters.get_mut(&completion.child) {
                owners.remove(requester);
                if owners.is_empty() {
                    state.shutdown_requesters.remove(&completion.child);
                }
            }
        }
        Ok(true)
    }

    pub(crate) async fn shutdown(&self, sender: &str, id: &str, key: &str) -> Result<()> {
        if id.starts_with("drv:") {
            return self.close_drivers(None, Some(crate::drivers::target(id)?.id)).await;
        }
        let source = self.open_actor(sender).await?;
        let target = self.open_actor(id).await?;
        let policy: Shutdown = {
            let reader = self.capability_reader(id).await?;
            serde_json::from_str(&actor::meta(&reader, "shutdown").await?)
                .with_context(|| format!("actor {id}: invalid shutdown policy"))?
        };
        {
            let mut conn = source.conn.lock().await;
            let tx = conn.transaction().await?;
            tx.execute(
                "INSERT INTO shutdowns(child,request) VALUES (?,?) ON CONFLICT(child) DO UPDATE SET request=excluded.request",
                [id, key],
            )
            .await?;
            self.commit_control(sender, tx).await?;
            let mut state = self.scheduling()?;
            state.shutdown_requesters.entry(id.into()).or_default().insert(sender.into());
            state.shutdown_dirty.insert(sender.into());
        }
        if matches!(policy, Shutdown::Brutal) {
            return self.stop_unlocked(id, "kill", key, sender).await;
        }
        let deadline = match policy {
            Shutdown::TimeoutMs(ms) => Some(crate::effects::now()?.checked_add(i64::try_from(ms)?).context("shutdown deadline overflow")?),
            Shutdown::Infinity => None,
            Shutdown::Brutal => {
                anyhow::bail!("actor {id} seq -1: brutal shutdown reached graceful dispatch")
            }
        };
        let mut conn = if let Some(deadline) = deadline {
            let remaining = u64::try_from(deadline.saturating_sub(crate::effects::now()?).max(0))?;
            match tokio::time::timeout(std::time::Duration::from_millis(remaining), target.conn.lock()).await {
                Ok(conn) => conn,
                Err(_) => return self.stop_unlocked(id, "kill", key, sender).await,
            }
        } else {
            target.conn.lock().await
        };
        if actor::status(&conn).await? == Status::Stopped {
            drop(conn);
            return self.sync_shutdowns(id).await;
        }
        if applied(&conn, key).await? {
            return Ok(());
        }
        let trapping: bool = actor::meta(&conn, "trap_exit").await?.parse()?;
        if !trapping || actor::status(&conn).await? == Status::Parked {
            drop(conn);
            if let Some(deadline) = deadline {
                let remaining = u64::try_from(deadline.saturating_sub(crate::effects::now()?).max(0))?;
                return match tokio::time::timeout(
                    std::time::Duration::from_millis(remaining),
                    self.stop_unlocked(id, "shutdown", key, sender),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => self.stop_unlocked(id, "kill", key, sender).await,
                };
            }
            return self.stop_unlocked(id, "shutdown", key, sender).await;
        }
        let tx = conn.transaction().await?;
        let msg = serde_json::to_vec(
            &serde_json::json!({"type":"exit","from":sender,"reason":"shutdown","initiator":sender,"event":key,"generation":actor::meta(&tx,"generation").await?.parse::<i64>()?}),
        )?;
        actor::inject(&tx, key, sender, &msg).await?;
        actor::set_meta(&tx, "shutdown_pending", key).await?;
        if let Some(deadline) = deadline {
            tx.execute(
                "INSERT OR IGNORE INTO timers(ref,target,msg,deadline,kind,armed,initiator) VALUES (?,?,?,?,'shutdown',1,?)",
                turso::params![key, id, &[] as &[u8], deadline, sender],
            )
            .await?;
        }
        actor::set_meta(&tx, &format!("applied:{key}"), "1").await?;
        self.commit_control(id, tx).await?;
        if let Some(deadline) = deadline {
            self.shutdown_deadlines.lock().await.insert(
                key.to_owned(),
                crate::messaging::ShutdownTimer {
                    reference: key.to_owned(),
                    target: id.to_owned(),
                    initiator: sender.to_owned(),
                    deadline,
                },
            );
        }
        drop(conn);
        self.request_timer_scan()?;
        self.wake_actor(id)?;
        Ok(())
    }

    pub async fn restart(&self, id: &str, verb: RestartVerb) -> Result<()> {
        let _admission = self.admit().await?;
        self.pump_unlocked(id).await?;
        let restarted = self
            .restart_unlocked(id, verb, &format!("host:{}", ulid::Ulid::new()))
            .await
            .with_context(|| format!("actor {id} seq -1: restart"))?;
        ensure!(restarted, "actor {id} seq -1: graceful shutdown or outbox delivery is still pending");
        let owner = self.open_actor(id).await?;
        let mut conn = owner.conn.lock().await;
        let tx = conn.transaction().await?;
        actor::set_meta(&tx, "ready", "true").await?;
        self.commit_control(id, tx).await?;
        self.wake_actor(id)?;
        Ok(())
    }

    pub(crate) async fn restart_unlocked(&self, id: &str, verb: RestartVerb, key: &str) -> Result<bool> {
        let _lifecycle = self.guard(&format!("lifecycle:{id}")).await;
        let actor = self.open_actor(id).await?;
        {
            let conn = actor.conn.lock().await;
            if applied(&conn, key).await? {
                return Ok(true);
            }
            if actor::status(&conn).await? != Status::Stopped
                && !actor::query(&conn, "SELECT value FROM meta WHERE key='shutdown_pending'", ()).await?.rows.is_empty()
            {
                return Ok(false);
            }
            ensure!(
                actor::query(&conn, "SELECT value FROM meta WHERE key='replay_source'", ()).await?.rows.is_empty(),
                "fork cannot be restarted into live delivery"
            );
        }
        let mut conn = actor.conn.lock().await;
        if !actor::query(&conn, "SELECT seq FROM outbox WHERE delivered=0 LIMIT 1", ()).await?.rows.is_empty() {
            return Ok(false);
        }
        if verb == RestartVerb::Reset {
            self.close_drivers(Some(id), None).await?;
            self.reset(&mut conn, id, key).await?;
            {
                let mut state = self.scheduling()?;
                state.index_dirty.insert(id.into());
                state.shutdown_dirty.remove(id);
                state.shutdown_requesters.retain(|_, owners| {
                    owners.remove(id);
                    !owners.is_empty()
                });
            }
            self.shutdown_deadlines.lock().await.retain(|_, timer| timer.target != id);
            self.request_timer_scan()?;
            return Ok(true);
        }
        let tx = conn.transaction().await?;
        if verb == RestartVerb::Skip {
            let poison_seq: i64 = actor::meta(&tx, "poison_seq").await?.parse()?;
            let rows = actor::query(&tx, "SELECT msg,state FROM inbox WHERE seq=?", [poison_seq]).await?;
            let row = rows.rows.first().context("poison message missing")?;
            if row.get::<String>(1)? != "done" {
                tx.execute(
                    "INSERT OR IGNORE INTO dead_letters(seq,msg,error,at) VALUES (?,?,?,?)",
                    turso::params![poison_seq, row.get::<Vec<u8>>(0)?, "supervisor skip", crate::effects::now()?],
                )
                .await?;
                crate::mailbox::complete(&tx, poison_seq).await?;
                actor::set_meta(&tx, &format!("skipped:{poison_seq}"), "1").await?;
                actor::set_meta(&tx, &format!("code_at:{poison_seq}"), &actor::code(&tx).await?.revision.to_string()).await?;
            }
        }
        tx.execute("DELETE FROM meta WHERE key='shutdown_pending'", ()).await?;
        actor::set_meta(&tx, "status", "running").await?;
        actor::set_meta(&tx, "ready", "false").await?;
        actor::set_meta(&tx, "reason", "").await?;
        actor::set_meta(&tx, &format!("applied:{key}"), "1").await?;
        self.commit_control(id, tx).await?;
        self.scheduling()?.index_dirty.insert(id.into());
        self.shutdown_deadlines.lock().await.retain(|_, timer| timer.target != id);
        Ok(true)
    }
}
