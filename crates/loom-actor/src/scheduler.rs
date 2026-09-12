//! Independent actor turns and pump tasks keep a blocked actor local to its file.
use crate::{ActorId, Node};
use anyhow::Result;
use std::collections::{BTreeSet, HashMap};

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
    pub(crate) async fn run_until_idle_inner(&self) -> Result<usize> {
        let mut processed = 0;
        let mut jobs = tokio::task::JoinSet::new();
        let mut owners = HashMap::new();
        let mut running = BTreeSet::new();
        let mut idle = BTreeSet::new();
        let mut dirty = BTreeSet::new();
        let mut timer_running = false;
        let mut deadline = None;
        let mut timer_scan_needed = true;
        let mut failure = None;
        loop {
            if failure.is_none() {
                for id in self.actor_ids()? {
                    if running.contains(&id) || idle.contains(&id) {
                        continue;
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
                            Ok(moved) => node.pump_unlocked(&actor_id).await.map(|pumped| Progress {
                                moved: moved || pumped,
                                processed: usize::from(moved),
                                deadline: None,
                            }),
                            Err(error) => Err(error),
                        };
                        Finished { task: tokio::task::id(), owner: task_owner, result }
                    });
                    owners.insert(abort.id(), owner);
                    running.insert(id.clone());
                    self.tasks.lock().await.insert(id, cancellation);
                    let _ = start_tx.send(());
                }
                if timer_scan_needed && !timer_running {
                    timer_scan_needed = false;
                    timer_running = true;
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
                if let Some(at) = deadline {
                    let delay = u64::try_from(i64::saturating_sub(at, crate::effects::now()?).max(0))?;
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {},
                        _ = self.wake.notified() => { idle.clear(); },
                    }
                    timer_scan_needed = true;
                    continue;
                }
                return Ok(processed);
            }
            let completed = tokio::select! {
                completed = jobs.join_next() => completed,
                _ = self.wake.notified(), if failure.is_none() => { idle.clear(); dirty.extend(running.iter().cloned()); timer_scan_needed = true; continue; },
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)), if failure.is_none() && !timer_running => {
                    timer_scan_needed = true;
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
                            running.remove(id);
                            self.tasks.lock().await.remove(id);
                        }
                        Owner::Timers => timer_running = false,
                    }
                    match done.result {
                        Ok(progress) => {
                            processed += progress.processed;
                            if matches!(done.owner, Owner::Timers) {
                                deadline = progress.deadline;
                            }
                            if progress.moved {
                                idle.clear();
                                dirty.extend(running.iter().cloned());
                                timer_scan_needed = true;
                            } else if let Owner::Actor { id } = done.owner
                                && !dirty.remove(&id)
                            {
                                idle.insert(id);
                            }
                        }
                        Err(error) => {
                            failure = Some(error);
                            for signal in self.tasks.lock().await.values() {
                                signal.notify_one();
                            }
                        }
                    }
                }
                Err(error) => {
                    if let Some(owner) = owners.remove(&error.id()) {
                        match owner {
                            Owner::Actor { id } => {
                                running.remove(&id);
                                self.tasks.lock().await.remove(&id);
                            }
                            Owner::Timers => timer_running = false,
                        }
                    }
                    if !error.is_cancelled() {
                        failure = Some(error.into());
                        for signal in self.tasks.lock().await.values() {
                            signal.notify_one();
                        }
                    }
                    idle.clear();
                    timer_scan_needed = true;
                }
            }
        }
    }
}
