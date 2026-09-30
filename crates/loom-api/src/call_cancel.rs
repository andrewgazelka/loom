//! Cancelling a call by the id its client gave it.
//!
//! `eval` and `run` take an optional `call_id`. While such a call is running, `cancel {call_id}` stops it:
//! the call's future is dropped, which aborts a guest that is running (the runtime cancels an execution whose
//! call future goes away, and its epoch deadline callback interrupts wasm that is mid-loop) and releases
//! everything it held; the caller gets an error reply. A live slider is the use: each new value cancels the
//! job for the previous one.
//!
//! Limits, stated: an `eval` whose build has started lets the build finish in the background (its result is
//! cached for the next call; killing the compiler mid-build would throw away the warm server) and abandons
//! only the run; one still queued behind another build is skipped before it builds; and a call is identified
//! within its tenant (the registry belongs to the tenant's service), by an id that any principal with the
//! execute scope can cancel. Ids should be unique per call.
use super::*;
use std::{
    collections::HashMap,
    future::Future,
    sync::{Mutex, atomic::AtomicBool},
};

#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    wake: tokio::sync::Notify,
}

/// What a running call can ask: has it been cancelled, and wake me when it is.
#[derive(Clone)]
pub(crate) struct CancelToken(Arc<CancelState>);

impl CancelToken {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
    pub(crate) async fn cancelled(&self) {
        loop {
            // Register for the wake-up before checking the flag, so a cancel between the two is not lost.
            let woken = self.0.wake.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            woken.await;
        }
    }
}

#[derive(Default)]
struct Registry {
    running: HashMap<String, Arc<CancelState>>,
    /// Ids cancelled before their call registered (requests can overtake each other), with when.
    early: HashMap<String, Instant>,
}

/// The calls running under a client-chosen id.
#[derive(Clone, Default)]
pub(crate) struct ActiveCalls(Arc<Mutex<Registry>>);

/// A cancel that arrives this long before its call is still honoured. Ids should be unique per call: an id
/// cancelled when nothing runs under it and started again within this window is cancelled at once.
const EARLY_CANCEL_TTL: Duration = Duration::from_secs(5);
const MAX_EARLY: usize = 1024;

/// Removes the id when the call ends, however it ends (finished, failed, cancelled, dropped).
struct Registered<'a> {
    calls: &'a ActiveCalls,
    id: String,
    state: Arc<CancelState>,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        let mut registry = self.calls.0.lock().unwrap_or_else(|e| e.into_inner());
        // Only our own entry: an id reused after this call ended belongs to the newer call.
        if registry.running.get(&self.id).is_some_and(|state| Arc::ptr_eq(state, &self.state)) {
            registry.running.remove(&self.id);
        }
    }
}

const MAX_CALL_ID: usize = 128;

impl Service {
    /// Run `work`, stoppable by `cancel` when `args` carry a `call_id`; plain `work` otherwise.
    pub(crate) async fn cancellable<T>(
        &self,
        args: &Value,
        work: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        self.cancellable_with(args, |_| work).await
    }

    /// [`Self::cancellable`] for work that wants to know about cancellation itself: `make` receives the call's
    /// token (`None` without a `call_id`). `eval` uses it to let a started build finish in the background
    /// while the run that would follow it is abandoned.
    pub(crate) async fn cancellable_with<T, F: Future<Output = Result<T>>>(
        &self,
        args: &Value,
        make: impl FnOnce(Option<CancelToken>) -> F,
    ) -> Result<T> {
        let Some(id) = args.get("call_id").filter(|value| !value.is_null()) else {
            return make(None).await;
        };
        let id = id.as_str().context("call_id must be a string")?.to_owned();
        ensure!(
            !id.is_empty() && id.len() <= MAX_CALL_ID,
            "call_id is 1 to {MAX_CALL_ID} bytes"
        );
        let state = Arc::new(CancelState::default());
        {
            let mut registry = self.calls.0.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(!registry.running.contains_key(&id), "call_id {id:?} is already running");
            if let Some(noted) = registry.early.remove(&id)
                && noted.elapsed() < EARLY_CANCEL_TTL
            {
                anyhow::bail!("call {id:?} cancelled before it started");
            }
            registry.running.insert(id.clone(), state.clone());
        }
        let _registered = Registered { calls: &self.calls, id: id.clone(), state: state.clone() };
        let token = CancelToken(state);
        let work = make(Some(token.clone()));
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => result,
            () = token.cancelled() => anyhow::bail!("call {id:?} cancelled"),
        }
    }

    /// `cancel {call_id}`: stop the running call with that id. `cancelled` is false when none is running
    /// (it finished, or has not started): not an error, because a client cancels speculatively. A cancel for
    /// an id that has not started yet is remembered for a few seconds (`noted`), since requests can
    /// overtake each other.
    pub(crate) fn cancel_call(&self, args: &Value) -> Result<Value> {
        let id = field(args, "call_id")?;
        let mut registry = self.calls.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = registry.running.get(id).cloned() {
            drop(registry);
            state.cancelled.store(true, Ordering::Release);
            state.wake.notify_waiters();
            return Ok(json!({"call_id": id, "cancelled": true}));
        }
        registry.early.retain(|_, at| at.elapsed() < EARLY_CANCEL_TTL);
        if registry.early.len() < MAX_EARLY {
            registry.early.insert(id.to_owned(), Instant::now());
        }
        Ok(json!({"call_id": id, "cancelled": false, "noted": true}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> Service {
        Service::new(
            Store::memory().unwrap(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            vec![],
        )
        .unwrap()
    }

    /// Work that would run for a minute unless it is dropped; `dropped` says it was.
    async fn slow(dropped: Arc<AtomicBool>) -> Result<u32> {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let _flag = Flag(dropped);
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok(1)
    }

    #[tokio::test]
    async fn cancel_drops_the_running_work_and_frees_the_id() {
        let service = service();
        let args = json!({"call_id": "slider-7"});
        let dropped = Arc::new(AtomicBool::new(false));
        let canceller = {
            let service = service.clone();
            tokio::spawn(async move {
                // The call registers itself first; wait for that, as a client would see "running".
                for _ in 0..200 {
                    if service.cancel_call(&json!({"call_id": "slider-7"})).unwrap()["cancelled"] == true {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                panic!("the call never registered");
            })
        };
        let started = Instant::now();
        let error = service.cancellable(&args, slow(dropped.clone())).await.unwrap_err();
        canceller.await.unwrap();
        assert!(error.to_string().contains("cancelled"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(dropped.load(Ordering::SeqCst), "the work future was dropped, which aborts a running guest");
        // The id is free again, and cancelling it now cancels nothing (it is only noted, for a call that
        // might still be on its way). A fresh id is unaffected.
        assert_eq!(service.cancel_call(&args).unwrap()["cancelled"], false);
        let again = service
            .cancellable(&json!({"call_id": "slider-8"}), async { Ok(7) })
            .await
            .unwrap();
        assert_eq!(again, 7);
    }

    #[tokio::test]
    async fn a_cancel_that_arrives_before_its_call_still_cancels_it_once() {
        let service = service();
        let args = json!({"call_id": "early"});
        let noted = service.cancel_call(&args).unwrap();
        assert_eq!(noted["cancelled"], false);
        assert_eq!(noted["noted"], true);
        let error = service.cancellable(&args, async { Ok(1) }).await.unwrap_err();
        assert!(error.to_string().contains("cancelled before it started"), "{error:#}");
        // The note is used up: the next call under that id runs.
        assert_eq!(service.cancellable(&args, async { Ok(2) }).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn work_that_takes_the_token_sees_the_cancel() {
        let service = service();
        let args = json!({"call_id": "token"});
        let canceller = {
            let service = service.clone();
            tokio::spawn(async move {
                for _ in 0..200 {
                    if service.cancel_call(&json!({"call_id": "token"})).unwrap()["cancelled"] == true {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
        };
        let seen = Arc::new(AtomicBool::new(false));
        let flag = seen.clone();
        let result = service
            .cancellable_with(&args, |token| async move {
                let token = token.expect("a call_id gives a token");
                token.cancelled().await;
                flag.store(true, Ordering::SeqCst);
                Ok(0)
            })
            .await;
        canceller.await.unwrap();
        // Either the work noticed first or the wrapper did: the call ends cancelled or with the work's value,
        // and the token was delivered.
        assert!(result.is_err() || seen.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_running_id_cannot_be_reused_and_work_without_an_id_is_untouched() {
        let service = service();
        let args = json!({"call_id": "x"});
        let first = {
            let service = service.clone();
            let args = args.clone();
            tokio::spawn(async move {
                service
                    .cancellable(&args, async {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        Ok(1)
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let error = service.cancellable(&args, async { Ok(2) }).await.unwrap_err();
        assert!(error.to_string().contains("already running"), "{error:#}");
        assert_eq!(first.await.unwrap().unwrap(), 1);
        assert_eq!(service.cancellable(&json!({}), async { Ok(3) }).await.unwrap(), 3);
        assert!(service.cancellable(&json!({"call_id": 5}), async { Ok(4) }).await.is_err());
        assert!(service.cancellable(&json!({"call_id": ""}), async { Ok(4) }).await.is_err());
    }
}

#[cfg(test)]
mod run_many_tests {
    use super::*;

    fn service() -> Service {
        Service::new(
            Store::memory().unwrap(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            vec![],
        )
        .unwrap()
    }
    async fn run_many(service: &Service, args: Value) -> loom_proto::Response {
        service
            .command(loom_proto::CommandRequest { session: None, command: "run_many".into(), args })
            .await
    }

    #[tokio::test]
    async fn run_many_isolates_failures_per_call_and_validates_the_batch() {
        let service = service();
        // Two calls that name nothing: each fails on its own, the batch still answers.
        let reply = run_many(&service, json!({"calls": [{"target": "nope", "args": []}, {"target": "also-nope"}]})).await;
        assert!(reply.ok, "{reply:?}");
        assert_eq!(reply.result["failures"], 2);
        assert_eq!(reply.result["results"].as_array().unwrap().len(), 2);
        assert_eq!(reply.result["results"][0]["ok"], false);
        // An empty batch is an empty answer.
        let reply = run_many(&service, json!({"calls": []})).await;
        assert!(reply.ok && reply.result["results"].as_array().unwrap().is_empty(), "{reply:?}");
        // Shape and bounds are errors for the whole request.
        assert!(!run_many(&service, json!({"calls": "x"})).await.ok);
        assert!(!run_many(&service, json!({"calls": [], "parallel": 0})).await.ok);
        assert!(!run_many(&service, json!({"calls": [], "parallel": 65})).await.ok);
        let too_many: Vec<Value> = (0..4097).map(|_| json!({"target": "x"})).collect();
        assert!(!run_many(&service, json!({"calls": too_many})).await.ok);
    }
}
