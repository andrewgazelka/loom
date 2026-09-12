//! Lexically scoped tasks. Task storage belongs to the scope, never the handle.
use crate::EffectError;
use std::{
    cell::{RefCell, UnsafeCell},
    marker::PhantomData,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

pub struct Scope<'scope, 'env: 'scope> {
    registry: &'scope TaskRegistry,
    scope: PhantomData<&'scope mut &'scope ()>,
    environment: PhantomData<&'env mut &'env ()>,
}

struct TaskRegistry {
    tasks: RefCell<Vec<TaskRecord>>,
}

struct TaskRecord {
    pointer: *mut (),
    cleanup: unsafe fn(*mut ()),
}

struct ResultSlot<T> {
    result: UnsafeCell<Option<T>>,
    published: AtomicBool,
    joined: AtomicBool,
    id: AtomicU64,
}

struct Task<F, T> {
    closure: UnsafeCell<Option<F>>,
    slot: ResultSlot<T>,
}

pub struct ScopedJoinHandle<'scope, T> {
    slot: &'scope ResultSlot<T>,
}

/// Run tasks borrowing caller-owned data; every task is joined before returning.
/// The scope stays on its owner; a child can create its own nested scope.
/// Use `scope` for borrowed captures, or [`crate::spawn`] for fire-and-forget
/// work and handles moved across tasks. Detached tasks still running when the
/// definition entry returns are cancelled; scoped tasks must finish first.
///
/// ```compile_fail
/// let mut escaped = None;
/// loom_guest_rs::scope(|scope| {
///     escaped = Some(scope.spawn(|| 1).unwrap());
/// });
/// ```
/// Non-Send captures and results are rejected independently.
///
/// ```compile_fail
/// let value = std::rc::Rc::new(1);
/// loom_guest_rs::scope(|scope| {
///     scope.spawn(move || *value).unwrap();
/// });
/// ```
///
/// ```compile_fail
/// loom_guest_rs::scope(|scope| {
///     scope.spawn(|| std::rc::Rc::new(1)).unwrap();
/// });
/// ```
pub fn scope<'env, F, R>(body: F) -> R
where
    F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> R,
{
    let registry = TaskRegistry {
        tasks: RefCell::new(Vec::new()),
    };
    let scope = Scope {
        registry: &registry,
        scope: PhantomData,
        environment: PhantomData,
    };
    let result = body(&scope);
    drop(registry);
    result
}

impl<'scope, 'env> Scope<'scope, 'env> {
    pub fn spawn<F, T>(&'scope self, closure: F) -> Result<ScopedJoinHandle<'scope, T>, EffectError>
    where
        F: FnOnce() -> T + Send + 'scope,
        T: Send + 'scope,
    {
        let task = Box::new(Task {
            closure: UnsafeCell::new(Some(closure)),
            slot: ResultSlot {
                result: UnsafeCell::new(None),
                published: AtomicBool::new(false),
                joined: AtomicBool::new(false),
                id: AtomicU64::new(0),
            },
        });
        let pointer = Box::into_raw(task);
        // Register before host scheduling: allocation failure or unwind cannot
        // leave an unowned task borrowing caller storage.
        self.registry.tasks.borrow_mut().push(TaskRecord {
            pointer: pointer.cast(),
            cleanup: cleanup::<F, T>,
        });
        // SAFETY: the scope retains this allocation until join completes. Only
        // the single task consumes F and writes T; publication gates readers.
        let id = unsafe { start_task(run::<F, T>, pointer.cast(), false) };
        if id == 0 {
            // SAFETY: a refused spawn guarantees the task never started.
            self.registry.tasks.borrow_mut().pop();
            unsafe {
                drop(Box::from_raw(pointer));
            }
            return Err("shared task unavailable or capacity exceeded".into());
        }
        // SAFETY: only the owning scope writes id; the trampoline never reads it.
        unsafe {
            (*pointer).slot.id.store(id, Ordering::Relaxed);
        }
        // SAFETY: scope owns storage for the invariant 'scope lifetime, even if
        // this handle is forgotten. It cannot escape the higher-ranked scope.
        Ok(ScopedJoinHandle {
            slot: unsafe { &(*pointer).slot },
        })
    }
}

impl<T> ScopedJoinHandle<'_, T> {
    pub fn join(self) -> Result<T, EffectError> {
        join_slot(self.slot);
        // SAFETY: task completed, acquire publication observed, and consuming
        // the non-Clone handle provides the sole result extraction.
        unsafe { (*self.slot.result.get()).take() }
            .ok_or_else(|| "task result already taken".into())
    }
}

fn join_slot<T>(slot: &ResultSlot<T>) {
    if !slot.joined.load(Ordering::Acquire) {
        if join_task(slot.id.load(Ordering::Relaxed)) != 0 {
            // A trapped child may have interrupted mutation of borrowed data.
            // Never resume Rust destructors or user code over that state.
            abort_execution();
        }
        if !slot.published.load(Ordering::Acquire) {
            abort_execution();
        }
        slot.joined.store(true, Ordering::Release);
    }
}

fn abort_execution() -> ! {
    #[cfg(target_arch = "wasm32")]
    {
        core::arch::wasm32::unreachable()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::process::abort()
    }
}

unsafe fn run<F: FnOnce() -> T, T>(pointer: *mut ()) {
    // SAFETY: host invokes this trampoline exactly once with its registered
    // allocation; scope cannot reclaim it until host join completes.
    let task = unsafe { &*pointer.cast::<Task<F, T>>() };
    let closure = unsafe { (*task.closure.get()).take().unwrap() };
    let result = closure();
    unsafe {
        *task.slot.result.get() = Some(result);
    }
    task.slot.published.store(true, Ordering::Release);
}

unsafe fn cleanup<F, T>(pointer: *mut ()) {
    // SAFETY: this is the sole scope-owned cleanup and runs after task joining.
    let task = unsafe { &*pointer.cast::<Task<F, T>>() };
    if task.slot.id.load(Ordering::Relaxed) != 0 {
        join_slot(&task.slot);
    }
    unsafe {
        drop(Box::from_raw(pointer.cast::<Task<F, T>>()));
    }
}

impl Drop for TaskRegistry {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().drain(..) {
            // SAFETY: each successfully spawned allocation has exactly one record.
            unsafe {
                (task.cleanup)(task.pointer);
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) unsafe fn start_task(run: unsafe fn(*mut ()), data: *mut (), detached: bool) -> u64 {
    unsafe {
        crate::core::host_spawn(
            run as usize as u32,
            data as usize as u32,
            i32::from(detached),
        )
    }
}
#[cfg(target_arch = "wasm32")]
pub(crate) fn join_task(id: u64) -> i32 {
    unsafe { crate::core::host_join(id) }
}
#[cfg(all(test, not(target_arch = "wasm32")))]
mod native_test {
    use std::{
        collections::BTreeMap,
        sync::{
            Mutex,
            atomic::{AtomicU64, Ordering},
        },
        thread::JoinHandle,
    };
    struct NativeTask {
        run: unsafe fn(*mut ()),
        data: *mut (),
    }
    // SAFETY: spawn's caller enforces F/T: Send and retains the allocation until
    // join; this wrapper transfers that exact pointer without losing provenance.
    unsafe impl Send for NativeTask {}
    impl NativeTask {
        fn execute(self) {
            unsafe {
                (self.run)(self.data);
            }
        }
    }
    static TASKS: Mutex<BTreeMap<u64, JoinHandle<()>>> = Mutex::new(BTreeMap::new());
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    pub unsafe fn start_task(run: unsafe fn(*mut ()), data: *mut ()) -> u64 {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let task = NativeTask { run, data };
        let handle = std::thread::spawn(move || task.execute());
        TASKS.lock().unwrap().insert(id, handle);
        id
    }
    pub fn join(id: u64) -> i32 {
        let handle = TASKS
            .lock()
            .unwrap()
            .remove(&id)
            .expect("missing test task");
        if handle.join().is_ok() { 0 } else { 1 }
    }
}
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) unsafe fn start_task(run: unsafe fn(*mut ()), data: *mut (), _detached: bool) -> u64 {
    unsafe { native_test::start_task(run, data) }
}
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) fn join_task(id: u64) -> i32 {
    native_test::join(id)
}

#[cfg(all(not(test), not(target_arch = "wasm32")))]
pub(crate) unsafe fn start_task(_run: unsafe fn(*mut ()), _data: *mut (), _detached: bool) -> u64 {
    0
}
#[cfg(all(not(test), not(target_arch = "wasm32")))]
pub(crate) fn join_task(_id: u64) -> i32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forgotten_result_is_dropped_by_its_scope() {
        struct ResultDrop<'a> {
            drops: &'a std::sync::atomic::AtomicUsize,
        }
        impl Drop for ResultDrop<'_> {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = std::sync::atomic::AtomicUsize::new(0);
        scope(|scope| {
            let _ =
                std::mem::ManuallyDrop::new(scope.spawn(|| ResultDrop { drops: &drops }).unwrap());
        });
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn scope_body_unwind_still_joins_borrowed_task() {
        let mut value = 0;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scope::<_, ()>(|scope| {
                let _ = std::mem::ManuallyDrop::new(
                    scope
                        .spawn(|| {
                            value = 9;
                        })
                        .unwrap(),
                );
                panic!("scope body control");
            });
        }));
        assert!(result.is_err());
        assert_eq!(value, 9);
    }

    #[test]
    fn borrowed_capture_and_forgotten_handle_are_joined() {
        let mut values = [1, 2, 3];
        scope(|scope| {
            let job = scope
                .spawn(|| {
                    values[1] = 8;
                    &values[1]
                })
                .unwrap();
            assert_eq!(*job.join().unwrap(), 8);
        });
        scope(|scope| {
            let _ = std::mem::ManuallyDrop::new(
                scope
                    .spawn(|| {
                        values[2] = 9;
                    })
                    .unwrap(),
            );
        });
        assert_eq!(values, [1, 8, 9]);
    }
}
