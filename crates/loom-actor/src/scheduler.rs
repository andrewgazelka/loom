//! Independent actor turns and pump tasks keep a blocked actor local to its file.
use crate::{ActorId, Node};
use anyhow::{Context, Result, anyhow};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Wakes leave `woken` only when admitted to a step. A wake during a step stays
/// pending until that step leaves `running` on completion. There is no idle set.
#[derive(Default)]
pub(crate) struct Scheduling {
    pub woken: BTreeSet<ActorId>,
    // Successful index synchronization removes this bit under the actor lock.
    pub index_dirty: BTreeSet<ActorId>,
    // A scanner consumes this flag; arming/recovery sets it again.
    pub timer_scan: bool,
    pub deadline: Option<i64>,
    // Scans replace each owner's earliest timer; cancellation/removal erases it.
    pub timer_deadlines: HashMap<ActorId, i64>,
    // A skipped locked owner leaves this set when its actor task completes.
    pub timer_deferred: BTreeSet<ActorId>,
    // Request rows leave on observed target completion, after their durable delete.
    pub shutdown_requesters: BTreeMap<ActorId, BTreeSet<ActorId>>,
    // A requester pump consumes this bit; a failed sync restores it.
    pub shutdown_dirty: BTreeSet<ActorId>,
    #[cfg(test)]
    pub step_attempts: HashMap<ActorId, usize>,
}

/// Completion removes each owner. Dropping a cancelled drain restores every
/// unfinished owner to the wake set before the next drain can acquire run_gate.
struct Running {
    node: Node,
    actors: BTreeSet<ActorId>,
    timers: bool,
}
impl Drop for Running {
    fn drop(&mut self) {
        if self.actors.is_empty() && !self.timers {
            return;
        }
        // Preserve pending work even on poisoning; the poisoned flag remains set
        // and the next scheduler operation reports the error.
        let mut state = self.node.scheduling.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.woken.extend(self.actors.iter().cloned());
        state.timer_scan = true;
        self.node.wake.notify_one();
    }
}

#[derive(Clone)]
enum Owner {
    Actor { id: ActorId },
    Timers,
}
struct Finished {
    task: tokio::task::Id,
    owner: Owner,
    result: Result<Progress>,
}
struct Progress {
    moved: bool,
    processed: usize,
    deadline: Option<i64>,
}

impl Node {
    /// Daemon-owned scheduler; shutdown drains admitted turns without aborting them.
    pub async fn run_service(&self, mut stop: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        loop {
            let wake = self.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if *stop.borrow() || stop.has_changed().is_err() || self.shipping.closed.load(std::sync::atomic::Ordering::Acquire) {
                return Ok(());
            }
            let result = {
                let _admission = self.admit().await?;
                let _run = self.run_gate.lock().await;
                self.run_until_idle_inner().await
            };
            if let Err(error) = result {
                eprintln!("actor <node> seq -1: cluster scheduler: {error:#}");
                // Failed destinations retry on this bounded tick; stop removes the wait.
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
                    _ = stop.changed() => {},
                }
            } else {
                tokio::select! {
                    _ = wake => {},
                    _ = stop.changed() => {},
                }
            }
        }
    }

    pub(crate) fn scheduling(&self) -> Result<std::sync::MutexGuard<'_, Scheduling>> {
        self.scheduling.lock().map_err(|_| anyhow!("scheduler state poisoned"))
    }

    /// Only actor ids enter the wake set: a malformed id (a driver id, a name)
    /// fails here at the caller, never two calls later inside run_until_idle.
    pub(crate) fn wake_actor(&self, id: &str) -> Result<()> {
        crate::ids::check(id).with_context(|| format!("wake_actor {id}"))?;
        self.scheduling()?.woken.insert(id.into());
        self.wake.notify_one();
        Ok(())
    }

    pub(crate) fn request_timer_scan(&self) -> Result<()> {
        self.scheduling()?.timer_scan = true;
        self.wake.notify_one();
        Ok(())
    }

    async fn check_timer_change(&self, id: &str) -> Result<()> {
        let known = self.scheduling()?.timer_deadlines.get(id).copied();
        if let Some(known) = known {
            let actor = self.open_actor(id).await?;
            let conn = actor.conn.lock().await;
            let rows = crate::actor::query(&conn, "SELECT deadline FROM timers WHERE armed=1 ORDER BY deadline LIMIT 1", ()).await?;
            let current = rows.rows.first().map(|row| row.get::<i64>(0)).transpose()?;
            if current != Some(known) {
                self.request_timer_scan()?;
            }
        }
        Ok(())
    }

    /// Return only after all woken actors have drained their deliverable outboxes
    /// and runnable inboxes and no due timer remains. Parked, deferred, stopped,
    /// fork and unpublished inbox rows retain their existing lifecycle semantics.
    /// Every committed source of runnable work must name its actor in `woken`;
    /// a missed wake is a correctness bug, never repaired by polling the roster.
    pub(crate) async fn run_until_idle_inner(&self) -> Result<usize> {
        let mut processed = 0;
        let mut jobs = tokio::task::JoinSet::new();
        let mut owners = HashMap::new();
        let mut running = Running { node: self.clone(), actors: BTreeSet::new(), timers: false };

        let mut failure = None;
        loop {
            if failure.is_none() {
                let ready: Vec<_> = self.scheduling()?.woken.difference(&running.actors).cloned().collect();
                for id in ready {
                    // A wake names an actor; only its owner steps it. A remote-owned actor's work reaches it
                    // through the ingress (docs/multi-node.md), so its wake is dropped here.
                    if matches!(self.resolve(&id).await?, crate::Placement::Remote { .. }) {
                        self.scheduling()?.woken.remove(&id);
                        continue;
                    }
                    running.actors.insert(id.clone());
                    {
                        let mut state = self.scheduling()?;
                        state.woken.remove(&id);
                        #[cfg(test)]
                        {
                            *state.step_attempts.entry(id.clone()).or_default() += 1;
                        }
                    }
                    let node = self.clone();
                    let actor_id = id.clone();
                    let owner = Owner::Actor { id: id.clone() };
                    let task_owner = owner.clone();
                    let start = tokio::sync::oneshot::channel::<()>();
                    let start_tx = start.0;
                    let start_rx = start.1;
                    let cancellation = std::sync::Arc::new(tokio::sync::Notify::new());
                    let task_cancellation = cancellation.clone();
                    let abort = jobs.spawn(async move {
                        let _ = start_rx.await;
                        let result = node.step(&actor_id, &task_cancellation).await;
                        node.tasks.lock().await.remove(&actor_id);
                        let result = match result {
                            Ok(moved) => async {
                                node.check_timer_change(&actor_id).await?;
                                let pumped = node.pump_unlocked(&actor_id).await?;
                                let again = if moved || pumped {
                                    let actor = node.open_actor(&actor_id).await?;
                                    let conn = actor.conn.lock().await;
                                    let rows = crate::actor::query(&conn,
                                        "SELECT (EXISTS(SELECT 1 FROM inbox WHERE state!='done') AND (SELECT value FROM meta WHERE key='status')='running' AND (SELECT value FROM meta WHERE key='ready')='true') OR EXISTS(SELECT 1 FROM outbox WHERE delivered=0)", ()).await?;
                                    rows.rows.first().ok_or_else(|| anyhow!("actor {actor_id}: missing work probe"))?.get::<i64>(0)? != 0
                                } else {
                                    false
                                };
                                Ok(Progress { moved: again, processed: usize::from(moved), deadline: None })
                            }.await,
                            Err(error) => Err(error),
                        };
                        Finished { task: tokio::task::id(), owner: task_owner, result }
                    });
                    owners.insert(abort.id(), owner);
                    self.tasks.lock().await.insert(id, cancellation);
                    let _ = start_tx.send(());
                }
                let scan = {
                    let mut state = self.scheduling()?;
                    let due = match state.deadline {
                        Some(at) => at <= crate::effects::now()?,
                        None => false,
                    };
                    if !running.timers && (state.timer_scan || due) {
                        state.timer_scan = false;
                        state.deadline = None;
                        true
                    } else {
                        false
                    }
                };
                if scan {
                    running.timers = true;
                    let node = self.clone();
                    let abort = jobs.spawn(async move {
                        let result = node.fire_timers().await.map(|timers| Progress {
                            moved: timers.progressed,
                            processed: 0,
                            deadline: timers.next_deadline,
                        });
                        Finished { task: tokio::task::id(), owner: Owner::Timers, result }
                    });
                    owners.insert(abort.id(), Owner::Timers);
                }
            }
            if jobs.is_empty() {
                self.tasks.lock().await.clear();
                if let Some(error) = failure {
                    return Err(error);
                }
                let deadline = {
                    let state = self.scheduling()?;
                    if !state.woken.is_empty() || state.timer_scan {
                        continue;
                    }
                    state.deadline
                };
                if let Some(at) = deadline {
                    let delay = u64::try_from(i64::saturating_sub(at, crate::effects::now()?).max(0))?;
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {},
                        _ = self.wake.notified() => {},
                    }
                    continue;
                }
                return Ok(processed);
            }
            let timer_wait = async {
                let deadline = self.scheduling()?.deadline;
                if let Some(at) = deadline {
                    let delay = u64::try_from(at.saturating_sub(crate::effects::now()?).max(0))?;
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                } else {
                    std::future::pending::<()>().await;
                }
                Ok::<(), anyhow::Error>(())
            };
            let completed = tokio::select! {
                completed = jobs.join_next() => completed,
                _ = self.wake.notified(), if failure.is_none() => { continue; },
                result = timer_wait, if failure.is_none() && !running.timers => {
                    result?;
                    self.request_timer_scan()?;
                    continue;
                },
            };
            let Some(completed) = completed else {
                continue;
            };
            match completed {
                Ok(done) => {
                    owners.remove(&done.task);
                    match &done.owner {
                        Owner::Actor { id } => {
                            self.tasks.lock().await.remove(id);
                            let mut state = self.scheduling()?;
                            if state.timer_deferred.remove(id) {
                                state.timer_scan = true;
                            }
                        }
                        Owner::Timers => running.timers = false,
                    }
                    match done.result {
                        Ok(progress) => {
                            processed += progress.processed;
                            if matches!(done.owner, Owner::Timers) {
                                let mut state = self.scheduling()?;
                                state.deadline = progress.deadline;
                                // Completion may race a scanner recording a busy owner.
                                if state.timer_deferred.iter().any(|id| !running.actors.contains(id)) {
                                    state.timer_scan = true;
                                }
                            }
                            if progress.moved
                                && let Owner::Actor { id } = &done.owner
                            {
                                self.wake_actor(id)?;
                            }
                        }
                        Err(error) => {
                            match &done.owner {
                                Owner::Actor { id } => self.wake_actor(id)?,
                                Owner::Timers => self.request_timer_scan()?,
                            }
                            failure = Some(error);
                            for signal in self.tasks.lock().await.values() {
                                signal.notify_one();
                            }
                        }
                    }
                    if let Owner::Actor { id } = &done.owner {
                        running.actors.remove(id);
                    }
                }
                Err(error) => {
                    if let Some(owner) = owners.remove(&error.id()) {
                        match owner {
                            Owner::Actor { id } => {
                                running.actors.remove(&id);
                                self.wake_actor(&id)?;
                                self.tasks.lock().await.remove(&id);
                            }
                            Owner::Timers => running.timers = false,
                        }
                    }
                    if !error.is_cancelled() {
                        failure = Some(error.into());
                        for signal in self.tasks.lock().await.values() {
                            signal.notify_one();
                        }
                    }
                    self.request_timer_scan()?;
                }
            }
        }
    }
}
