use crate::{Result, guest};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) struct Control {
    deadline: Instant,
    failure: Mutex<Option<String>>,
    handle: Mutex<Option<v8::IsolateHandle>>,
    done: AtomicBool,
}

impl Control {
    pub(crate) fn new(timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            deadline: Instant::now() + timeout,
            failure: Mutex::new(None),
            handle: Mutex::new(None),
            done: AtomicBool::new(false),
        })
    }

    pub(crate) fn install(&self, handle: v8::IsolateHandle) {
        *self.handle.lock().unwrap() = Some(handle);
        // A caller can drop the future while the worker creates its isolate.
        // Recheck after publication so cancellation cannot miss this interval.
        let _ = self.check();
    }

    pub(crate) fn fail(&self, message: impl Into<String>) {
        self.failure
            .lock()
            .unwrap()
            .get_or_insert_with(|| message.into());
        if let Some(handle) = self.handle.lock().unwrap().as_ref() {
            handle.terminate_execution();
        }
    }

    pub(crate) fn check(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            self.fail("JavaScript execution timed out");
        }
        let failure = self.failure.lock().unwrap().clone();
        if let Some(message) = failure {
            if let Some(handle) = self.handle.lock().unwrap().as_ref() {
                handle.terminate_execution();
            }
            Err(guest(message))
        } else {
            Ok(())
        }
    }

    pub(crate) fn finish(&self) {
        self.done.store(true, Ordering::Release);
        self.handle.lock().unwrap().take();
    }

    pub(crate) fn done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

pub(crate) struct CancelOnDrop(pub(crate) Arc<Control>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.0.done() {
            self.0.fail("JavaScript execution cancelled");
        }
    }
}
