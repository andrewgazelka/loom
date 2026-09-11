use crate::{Ctx, Node, Status, Trap, actor, ids};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Alarm {
    timer_ref: String,
}
#[derive(Serialize, Deserialize)]
struct CallRequest {
    reference: String,
    msg: Vec<u8>,
    timeout_ms: u64,
}
#[derive(Default)]
pub(crate) struct TimerProgress {
    pub progressed: bool,
    pub next_deadline: Option<i64>,
}
#[derive(Clone)]
pub(crate) struct ShutdownTimer {
    pub reference: String,
    pub target: String,
    pub initiator: String,
    pub deadline: i64,
}
struct Timer {
    reference: String,
    target: String,
    msg: Vec<u8>,
    deadline: i64,
    kind: String,
    initiator: String,
}

impl Ctx<'_> {
    fn reference(&self, kind: &str, idx: i64) -> String {
        format!("{kind}:{}:{}:{idx}", ids::incarnation(self.actor_id, self.generation), self.seq)
    }

    async fn deadline(&mut self, ms: u64) -> Result<i64, Trap> {
        let delay = i64::try_from(ms).map_err(|e| self.runtime(e))?;
        self.now().await?.checked_add(delay).ok_or_else(|| self.runtime("timer deadline overflow"))
    }

    /// Persist a timer in this message transaction; its alarm is armed after commit.
    pub async fn send_after(&mut self, target: &str, ms: u64, msg: &[u8]) -> Result<String, Trap> {
        ids::check(target).map_err(|e| self.runtime(e))?;
        let deadline = self.deadline(ms).await?;
        let idx = self.next_index()?;
        let reference = self.reference("timer", idx);
        self.conn
            .execute(
                "INSERT INTO timers(ref,target,msg,deadline,kind,armed,initiator) VALUES (?,?,?,?,'message',0,?)",
                turso::params![reference.as_str(), target, msg, deadline, self.actor_id],
            )
            .await
            .map_err(|e| self.runtime(e))?;
        let request = serde_json::to_vec(&Alarm { timer_ref: reference.clone() }).map_err(|e| self.runtime(e))?;
        self.outbox(idx, "effect:alarm", &request).await?;
        Ok(reference)
    }

    /// Cancellation wins if the timer has not atomically entered the outbox yet.
    pub async fn cancel_timer(&mut self, reference: &str) -> Result<(), Trap> {
        self.conn.execute("DELETE FROM timers WHERE ref=?", [reference]).await.map_err(|e| self.runtime(e))?;
        Ok(())
    }

    pub async fn read_timer(&mut self, reference: &str) -> Result<Option<u64>, Trap> {
        let rows = self.sql("SELECT deadline FROM timers WHERE ref=?", [reference]).await?;
        let Some(row) = rows.rows.first() else {
            return Ok(None);
        };
        let deadline: i64 = row.get(0).map_err(|e| self.runtime(e))?;
        let remaining = deadline.saturating_sub(self.now().await?).max(0);
        Ok(Some(u64::try_from(remaining).map_err(|e| self.runtime(e))?))
    }

    /// The next handler receives exactly one reply, down, or call_timeout envelope.
    pub async fn call(&mut self, target: &str, msg: &[u8], timeout_ms: u64) -> Result<String, Trap> {
        ids::check(target).map_err(|e| self.runtime(e))?;
        let deadline = self.deadline(timeout_ms).await?;
        let idx = self.next_index()?;
        let reference = self.reference("call", idx);
        self.conn
            .execute("INSERT INTO calls(ref,target,timer_ref) VALUES (?,?,?)", [reference.as_str(), target, reference.as_str()])
            .await
            .map_err(|e| self.runtime(e))?;
        self.conn
            .execute(
                "INSERT INTO timers(ref,target,msg,deadline,kind,armed,initiator) VALUES (?,?,?,?,'call',0,?)",
                turso::params![reference.as_str(), target, &[] as &[u8], deadline, self.actor_id],
            )
            .await
            .map_err(|e| self.runtime(e))?;
        let request = serde_json::to_vec(&CallRequest { reference: reference.clone(), msg: msg.to_vec(), timeout_ms })
            .map_err(|e| self.runtime(e))?;
        self.outbox(idx, &format!("call:{target}"), &request).await?;
        Ok(reference)
    }

    pub async fn reply(&mut self, from: &str, reference: &str, msg: &[u8]) -> Result<(), Trap> {
        let reply = serde_json::to_vec(&serde_json::json!({"type":"reply","ref":reference,"msg":msg})).map_err(|e| self.runtime(e))?;
        self.send(from, &reply).await
    }
}

impl Node {
    pub(crate) async fn arm_alarm(&self, sender: &str, msg: &[u8]) -> Result<bool> {
        let payload: serde_json::Value = match serde_json::from_slice(msg) {
            Ok(payload) => payload,
            Err(_) => return Ok(false), // Ordinary alarm effects use arbitrary request bytes.
        };
        if payload.get("timer_ref").is_none() {
            return Ok(false);
        }
        let request: Alarm = serde_json::from_value(payload).with_context(|| format!("actor {sender} seq -1: invalid timer alarm"))?;
        let source = self.open(sender).await?;
        source.conn.lock().await.execute("UPDATE timers SET armed=1 WHERE ref=? AND kind='message'", [request.timer_ref]).await?;
        Ok(true)
    }

    /// The sender pump serializes call delivery; the monitor is installed before the call envelope.
    pub(crate) async fn deliver_call(&self, sender: &str, target: &str, msg: &[u8], key: &str) -> Result<()> {
        let request: CallRequest = serde_json::from_slice(msg)?;
        let source = self.open(sender).await?;
        {
            let conn = source.conn.lock().await;
            if !actor::query(&conn, "SELECT value FROM meta WHERE key=?", [format!("call_done:{}", request.reference)])
                .await?
                .rows
                .is_empty()
            {
                drop(conn);
                return self.demonitor(sender, &request.reference, target).await;
            }
            let calls = actor::query(&conn, "SELECT target FROM calls WHERE ref=?", [request.reference.as_str()]).await?;
            let call = calls.rows.first().context("call outbox has no calls row")?;
            ensure!(call.get::<String>(0)? == target, "actor {sender} seq -1: call target differs from persisted target");
        }
        self.monitor(sender, target, &request.reference).await?;
        let envelope = serde_json::to_vec(&serde_json::json!({"type":"call","ref":request.reference,"from":sender,"msg":request.msg}))?;
        self.deliver_message(target, key, sender, &envelope).await?;
        source.conn.lock().await.execute("UPDATE timers SET armed=1 WHERE ref=? AND kind='call'", [request.reference]).await?;
        Ok(())
    }

    /// The receipt and winning inbox insertion share one caller transaction.
    /// Retry also repeats monitor cleanup after a crash between the two files.
    pub(crate) async fn complete_call(&self, caller: &str, sender: &str, _key: &str, msg: &[u8]) -> Result<bool> {
        let payload = match serde_json::from_slice::<serde_json::Value>(msg) {
            Ok(value) => value,
            Err(_) => return Ok(false),
        };
        let kind = payload["type"].as_str().unwrap_or("");
        if !matches!(kind, "reply" | "down" | "call_timeout") {
            return Ok(false);
        }
        let Some(reference) = payload["ref"].as_str() else {
            return Ok(false);
        };
        let owner = self.open(caller).await?;
        let mut conn = owner.conn.lock().await;
        let tx = conn.transaction().await?;
        let receipt = format!("call_done:{reference}");
        let done = actor::query(&tx, "SELECT value FROM meta WHERE key=?", [receipt.as_str()]).await?;
        let target = if let Some(row) = done.rows.first() {
            row.get::<String>(0)?
        } else {
            let rows = actor::query(&tx, "SELECT target,timer_ref FROM calls WHERE ref=?", [reference]).await?;
            let Some(row) = rows.rows.first() else {
                // Reset retains monitors but discards calls. Drain the old incarnation's
                // monitor when its eventual outcome arrives, without invoking a handler.
                let stale = reference.starts_with("call:");
                let monitors = actor::query(&tx, "SELECT target FROM monitors WHERE ref=?", [reference]).await?;
                let target = monitors.rows.first().map(|row| row.get::<String>(0)).transpose()?;
                tx.rollback().await?;
                drop(conn);
                if stale && let Some(target) = target {
                    self.demonitor(caller, reference, &target).await?;
                }
                return Ok(stale);
            };
            let target: String = row.get(0)?;
            let timer_ref: String = row.get(1)?;
            ensure!(
                if kind == "call_timeout" { sender == caller } else { sender == target },
                "actor {caller} seq -1: response {reference} came from unexpected actor {sender}"
            );
            if kind == "call_timeout"
                && actor::query(&tx, "SELECT ref FROM timers WHERE ref=? AND armed=1", [timer_ref.as_str()]).await?.rows.is_empty()
            {
                tx.rollback().await?;
                return Ok(true);
            }
            actor::inject(&tx, &format!("call_result:{reference}"), sender, msg).await?;
            tx.execute("DELETE FROM calls WHERE ref=?", [reference]).await?;
            tx.execute("DELETE FROM timers WHERE ref=?", [timer_ref]).await?;
            actor::set_meta(&tx, &receipt, &target).await?;
            target
        };
        tx.commit().await?;
        drop(conn);
        self.demonitor(caller, reference, &target).await?;
        self.wake.notify_one();
        Ok(true)
    }

    pub(crate) async fn fire_shutdowns(&self) -> Result<bool> {
        let mut progressed = false;
        let known = self.shutdown_deadlines.lock().await.clone();
        for timer in known.values() {
            if timer.deadline > crate::effects::now()? {
                continue;
            }
            progressed |= self.kill_from_timer(&timer.target, &timer.initiator, &timer.reference).await?;
        }
        Ok(progressed)
    }

    pub(crate) async fn fire_timers(&self) -> Result<TimerProgress> {
        let mut progress = TimerProgress { progressed: self.fire_shutdowns().await?, next_deadline: None };
        for id in self.actor_ids()? {
            let source = self.open(&id).await?;
            let timers = {
                let Ok(conn) = source.conn.try_lock() else {
                    continue;
                };
                if actor::status(&conn).await? == Status::Fork
                    || !actor::query(&conn, "SELECT value FROM meta WHERE key='replay_source'", ()).await?.rows.is_empty()
                {
                    continue;
                }
                let rows = actor::query(
                    &conn,
                    "SELECT ref,target,msg,deadline,kind,initiator FROM timers WHERE armed=1 ORDER BY deadline,ref",
                    (),
                )
                .await?;
                let mut timers = Vec::new();
                for row in rows.rows {
                    let timer = Timer {
                        reference: row.get(0)?,
                        target: row.get(1)?,
                        msg: row.get(2)?,
                        deadline: row.get(3)?,
                        kind: row.get(4)?,
                        initiator: row.get(5)?,
                    };
                    if timer.kind == "shutdown" {
                        self.shutdown_deadlines.lock().await.insert(
                            timer.reference.clone(),
                            ShutdownTimer {
                                reference: timer.reference.clone(),
                                target: timer.target.clone(),
                                initiator: timer.initiator.clone(),
                                deadline: timer.deadline,
                            },
                        );
                    }
                    timers.push(timer);
                }
                timers
            };
            for timer in timers {
                if timer.deadline > crate::effects::now()? {
                    progress.next_deadline = Some(progress.next_deadline.map_or(timer.deadline, |old| old.min(timer.deadline)));
                    continue;
                }
                match timer.kind.as_str() {
                    "message" => {
                        let mut conn = source.conn.lock().await;
                        let tx = conn.transaction().await?;
                        let removed = tx.execute("DELETE FROM timers WHERE ref=? AND armed=1", [timer.reference.as_str()]).await?;
                        if removed != 0 {
                            actor::enqueue(&tx, actor::cursor(&tx).await?, &timer.target, &timer.msg).await?;
                        }
                        tx.commit().await?;
                        progress.progressed |= removed != 0;
                    }
                    "call" => {
                        let msg = serde_json::to_vec(&serde_json::json!({"type":"call_timeout","ref":timer.reference}))?;
                        progress.progressed |= self.complete_call(&id, &id, &timer.reference, &msg).await?;
                    }
                    "shutdown" => {
                        // Abort before waiting for the target connection. The lifecycle
                        // helper verifies the durable timer under that connection lock.
                        progress.progressed |= self.kill_from_timer(&timer.target, &timer.initiator, &timer.reference).await?;
                        continue;
                    }
                    kind => anyhow::bail!("actor {id} seq -1: unknown timer kind {kind:?}"),
                }
            }
        }
        Ok(progress)
    }
}
