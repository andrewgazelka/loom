//! Owned tasks whose handles can move between fibers of one execution.
use crate::{
    EffectError,
    scoped::{join_task, start_task},
};
use std::{
    cell::UnsafeCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

struct ResultSlot<T> {
    result: UnsafeCell<Option<T>>,
    published: AtomicBool,
}

struct Task<F, T> {
    closure: UnsafeCell<Option<F>>,
    slot: ResultSlot<T>,
}

// SAFETY: only the trampoline accesses the closure or writes the result. The
// sole consuming join reads after acquire publication. Last-owner destruction
// cannot overlap either operation. Neither F nor T is exposed by shared reference.
unsafe impl<F: Send, T: Send> Sync for Task<F, T> {}

trait TaskResult<T> {
    fn slot(&self) -> &ResultSlot<T>;
}
impl<F, T> TaskResult<T> for Task<F, T> {
    fn slot(&self) -> &ResultSlot<T> {
        &self.slot
    }
}

/// An owned task handle. Dropping it leaves the task running.
///
/// ```compile_fail
/// let handle = loom_guest_rs::spawn(|| 1).unwrap();
/// let duplicate = handle.clone();
/// ```
pub struct JoinHandle<T> {
    task: Arc<dyn TaskResult<T> + Send + Sync>,
    id: u64,
}

/// Spawn a task with owned, `'static` captures and result. Use this for
/// fire-and-forget work or handles moved across tasks; use [`crate::scope`] to
/// borrow caller data. Dropping the handle does not wait. Tasks still running
/// when the definition's entry returns are cancelled by the host.
pub fn spawn<F, T>(f: F) -> Result<JoinHandle<T>, EffectError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let task = Arc::new(Task {
        closure: UnsafeCell::new(Some(f)),
        slot: ResultSlot {
            result: UnsafeCell::new(None),
            published: AtomicBool::new(false),
        },
    });
    // One allocation, two owners. Erasing the handle's type does not allocate.
    let pointer = Arc::into_raw(task.clone());
    // SAFETY: the raw Arc reference belongs to the exactly-once trampoline.
    let id = unsafe { start_task(run_detached::<F, T>, pointer.cast_mut().cast(), true) };
    if id == 0 {
        // SAFETY: refusal guarantees the trampoline never acquired this owner.
        unsafe {
            drop(Arc::from_raw(pointer));
        }
        return Err("shared task unavailable or capacity exceeded".into());
    }
    Ok(JoinHandle { task, id })
}

impl<T> JoinHandle<T> {
    /// Wait for this task from any fiber in the same execution.
    pub fn join(self) -> Result<T, EffectError> {
        if join_task(self.id) != 0 {
            return Err(task_error(self.id));
        }
        let slot = self.task.slot();
        if !slot.published.load(Ordering::Acquire) {
            return Err("shared task completed without publishing its result".into());
        }
        // SAFETY: publication synchronizes the write; this non-Clone handle is
        // consumed, so no other caller can extract the result.
        unsafe { (*slot.result.get()).take() }.ok_or_else(|| "task result already taken".into())
    }
}

unsafe fn run_detached<F: FnOnce() -> T, T>(pointer: *mut ()) {
    // SAFETY: consumes precisely the raw strong reference transferred by spawn.
    let task = unsafe { Arc::from_raw(pointer.cast::<Task<F, T>>()) };
    let closure = unsafe { (*task.closure.get()).take().unwrap() };
    let result = closure();
    unsafe {
        *task.slot.result.get() = Some(result);
    }
    task.slot.published.store(true, Ordering::Release);
    // Arc drop releases the task owner, even on native unwind. Wasm traps do
    // not unwind: that reference remains in execution memory until teardown.
}

#[cfg(all(loom_core, target_arch = "wasm32"))]
fn task_error(id: u64) -> EffectError {
    // SAFETY: join_error transfers a UTF-8 allocation made with alignment 1.
    let packed = unsafe { crate::core::host_join_error(id) };
    let length = (packed >> 32) as usize;
    let bytes = unsafe { Vec::from_raw_parts(packed as u32 as *mut u8, length, length) };
    String::from_utf8(bytes).expect("invalid shared job error UTF-8")
}
#[cfg(not(target_arch = "wasm32"))]
fn task_error(id: u64) -> EffectError {
    format!("shared job {id}: task failed")
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn join_returns_value_and_handle_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<JoinHandle<String>>();
        assert_eq!(
            spawn(|| String::from("done")).unwrap().join().unwrap(),
            "done"
        );
    }

    #[test]
    fn handle_moves_into_scoped_child() {
        let handle = spawn(|| 42).unwrap();
        crate::scope(|scope| {
            assert_eq!(
                scope
                    .spawn(move || handle.join().unwrap())
                    .unwrap()
                    .join()
                    .unwrap(),
                42
            );
        });
    }

    #[test]
    fn dropped_handle_leaves_task_storage_alive() {
        struct Witness {
            dropped: std::sync::mpsc::Sender<()>,
        }
        impl Drop for Witness {
            fn drop(&mut self) {
                self.dropped.send(()).unwrap();
            }
        }
        let (release, wait) = std::sync::mpsc::channel();
        let (dropped, observed) = std::sync::mpsc::channel();
        let witness = Witness { dropped };
        let handle = spawn(move || {
            wait.recv().unwrap();
            witness
        })
        .unwrap();
        let id = handle.id;
        drop(handle);
        assert!(observed.try_recv().is_err());
        release.send(()).unwrap();
        assert_eq!(join_task(id), 0);
        observed.recv().unwrap();
    }

    #[test]
    fn failed_join_returns_error_and_other_tasks_continue() {
        let failed = spawn(|| panic!("detached failure")).unwrap();
        assert!(failed.join().unwrap_err().contains("shared job"));
        assert_eq!(spawn(|| 7).unwrap().join().unwrap(), 7);
    }
}
