use crate::{CacheStats, EffectRequest, Limits, Program, Result, control::Control, execution};
use std::sync::{
    Arc, Mutex, Once, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::Duration;
use tokio::sync::{mpsc as async_mpsc, oneshot};

pub(crate) enum Task {
    Compile {
        source: String,
        reply: oneshot::Sender<Result<Program>>,
    },
    Call {
        program: Arc<Program>,
        args: String,
        effects: async_mpsc::Sender<EffectRequest>,
        reply: oneshot::Sender<Result<serde_json::Value>>,
    },
}

pub(crate) struct Job {
    pub(crate) control: Arc<Control>,
    pub(crate) task: Task,
}

#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) compilations: AtomicU64,
    pub(crate) cache_hits: AtomicU64,
}

struct Watchdog {
    active: Mutex<Vec<Weak<Control>>>,
    stopped: AtomicBool,
}

pub(crate) struct Pool {
    pub(crate) limits: Limits,
    sender: mpsc::SyncSender<Job>,
    watchdog: Arc<Watchdog>,
    counters: Arc<Counters>,
}

impl Pool {
    pub(crate) fn new(limits: Limits) -> Result<Self> {
        static INITIALIZE: Once = Once::new();
        INITIALIZE.call_once(|| {
            let platform = v8::new_default_platform(2, false).make_shared();
            v8::V8::initialize_platform(platform);
            v8::V8::initialize();
        });
        let (sender, receiver) = mpsc::sync_channel::<Job>(limits.queue_capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let counters = Arc::new(Counters::default());
        let watchdog = Arc::new(Watchdog {
            active: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
        });
        for index in 0..limits.workers {
            let receiver = receiver.clone();
            let limits = limits.clone();
            let counters = counters.clone();
            std::thread::Builder::new()
                .name(format!("loom-v8-{index}"))
                .spawn(move || {
                    let worker = Worker {
                        receiver,
                        limits,
                        counters,
                    };
                    loop {
                        let job = worker.receiver.lock().unwrap().recv();
                        let Ok(job) = job else { break };
                        worker.run(job, 0);
                    }
                })?;
        }
        let monitor = watchdog.clone();
        std::thread::Builder::new()
            .name("loom-v8-watchdog".into())
            .spawn(move || {
                while !monitor.stopped.load(Ordering::Acquire) {
                    monitor.active.lock().unwrap().retain(|control| {
                        let Some(control) = control.upgrade() else {
                            return false;
                        };
                        if control.done() {
                            return false;
                        }
                        let _ = control.check();
                        true
                    });
                    std::thread::sleep(Duration::from_millis(5));
                }
            })?;
        Ok(Self {
            limits,
            sender,
            watchdog,
            counters,
        })
    }

    pub(crate) fn submit(&self, job: Job) -> Result<()> {
        self.watchdog
            .active
            .lock()
            .unwrap()
            .push(Arc::downgrade(&job.control));
        self.sender.try_send(job).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => anyhow::anyhow!("V8 worker queue is full"),
            mpsc::TrySendError::Disconnected(_) => anyhow::anyhow!("V8 worker pool stopped"),
        })
    }

    pub(crate) fn cache_stats(&self) -> CacheStats {
        CacheStats {
            compilations: self.counters.compilations.load(Ordering::Relaxed),
            cache_hits: self.counters.cache_hits.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.watchdog.stopped.store(true, Ordering::Release);
    }
}

/// A pending host effect can itself invoke this engine. Servicing queued work
/// while its isolate is suspended avoids consuming all workers in that chain.
/// Each nested isolate enters/exits on this same thread; no V8 handle migrates.
pub(crate) struct Worker {
    receiver: Arc<Mutex<mpsc::Receiver<Job>>>,
    limits: Limits,
    counters: Arc<Counters>,
}

impl Worker {
    pub(crate) fn run_pending(&self, depth: usize) -> bool {
        // Idle workers may be blocking in recv with the receiver lock. They
        // will take new work themselves; never block the active effect reply.
        let job = match self.receiver.try_lock() {
            Ok(receiver) => receiver.try_recv().ok(),
            Err(_) => None,
        };
        let Some(job) = job else { return false };
        self.run(job, depth + 1);
        true
    }

    fn run(&self, job: Job, depth: usize) {
        let admitted = depth < self.limits.max_reentrant_depth;
        match job.task {
            Task::Compile { source, reply } => {
                let result = if admitted {
                    execution::compile(&source, &self.limits, &job.control, &self.counters)
                } else {
                    Err(crate::guest("JavaScript reentrant call depth exceeded"))
                };
                job.control.finish();
                let _ = reply.send(result);
            }
            Task::Call {
                program,
                args,
                effects,
                reply,
            } => {
                let result = if admitted {
                    execution::call(
                        &program,
                        &args,
                        effects,
                        &self.limits,
                        &job.control,
                        &self.counters,
                        self,
                        depth,
                    )
                } else {
                    Err(crate::guest("JavaScript reentrant call depth exceeded"))
                };
                job.control.finish();
                let _ = reply.send(result);
            }
        }
    }
}
