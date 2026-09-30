//! Cancelling a call by the id its client gave it.
//!
//! `eval` and `run` take an optional `call_id`. While such a call is running, `cancel {call_id}` stops it:
//! the call's future is dropped, which aborts a guest that is running (the runtime cancels an execution whose
//! call future goes away, and its epoch deadline callback interrupts wasm that is mid-loop) and releases
//! everything it held; the caller gets an error reply. A live slider is the use: each new value cancels the
//! job for the previous one.
//!
//! Limits, stated: a build already handed to the compiler finishes (its result is cached for the next call);
//! cancelling a call that is still queued behind another build drops it before it starts; and a call is
//! identified within its tenant (the registry belongs to the tenant's service).
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

/// The calls running under a client-chosen id.
#[derive(Clone, Default)]
pub(crate) struct ActiveCalls(Arc<Mutex<HashMap<String, Arc<CancelState>>>>);

/// Removes the id when the call ends, however it ends (finished, failed, cancelled, dropped).
struct Registered<'a> {
    calls: &'a ActiveCalls,
    id: String,
    state: Arc<CancelState>,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        let mut calls = self.calls.0.lock().unwrap_or_else(|e| e.into_inner());
        // Only our own entry: an id reused after this call ended belongs to the newer call.
        if calls.get(&self.id).is_some_and(|state| Arc::ptr_eq(state, &self.state)) {
            calls.remove(&self.id);
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
        let Some(id) = args.get("call_id").filter(|value| !value.is_null()) else {
            return work.await;
        };
        let id = id.as_str().context("call_id must be a string")?.to_owned();
        ensure!(
            !id.is_empty() && id.len() <= MAX_CALL_ID,
            "call_id is 1 to {MAX_CALL_ID} bytes"
        );
        let state = Arc::new(CancelState::default());
        {
            let mut calls = self.calls.0.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(!calls.contains_key(&id), "call_id {id:?} is already running");
            calls.insert(id.clone(), state.clone());
        }
        let _registered = Registered {
            calls: &self.calls,
            id: id.clone(),
            state: state.clone(),
        };
        tokio::pin!(work);
        loop {
            // Register for the wake-up before checking the flag, so a cancel between the two is not lost.
            let woken = state.wake.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            if state.cancelled.load(Ordering::Acquire) {
                anyhow::bail!("call {id:?} cancelled");
            }
            tokio::select! {
                result = &mut work => return result,
                _ = &mut woken => {}
            }
        }
    }

    /// `cancel {call_id}`: stop the running call with that id. `cancelled` is false when none is running
    /// (it finished, or never started): not an error, because a client cancels speculatively.
    pub(crate) fn cancel_call(&self, args: &Value) -> Result<Value> {
        let id = field(args, "call_id")?;
        let state = self
            .calls
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned();
        match state {
            Some(state) => {
                state.cancelled.store(true, Ordering::Release);
                state.wake.notify_waiters();
                Ok(json!({"call_id": id, "cancelled": true}))
            }
            None => Ok(json!({"call_id": id, "cancelled": false})),
        }
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
        // The id is free again, and cancelling it now is a no-op, not an error.
        assert_eq!(service.cancel_call(&args).unwrap()["cancelled"], false);
        let again = service.cancellable(&args, async { Ok(7) }).await.unwrap();
        assert_eq!(again, 7);
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
