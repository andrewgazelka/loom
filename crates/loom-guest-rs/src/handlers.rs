//! Guest-defined deep handlers. The host serializes each frame's callbacks.
use crate::{EffectError, Value};
use serde::{Serialize, Deserialize, de::DeserializeOwned};
use std::{cell::UnsafeCell, sync::{Arc, atomic::{AtomicBool, AtomicU8, Ordering}}};

#[derive(Debug, Deserialize)]
pub struct Effect {
    pub name: String,
    pub args: Value,
}
impl Effect {
    pub fn arg<T: DeserializeOwned>(&self) -> Result<T, EffectError> {
        crate::decode_host(&crate::encode(&self.args)?)
    }
}

#[derive(Debug)]
pub enum Reply { Resume(Value), Forward, Deferred }

const CALLBACK: u8 = 1;
const DROPPED: u8 = 2;
const DISARMED: u8 = 4;
struct ContinuationState { id: u64, state: AtomicU8 }

/// A one-shot suspended performer. Dropping it reports `continuation dropped`.
///
/// ```compile_fail
/// fn duplicate(k: loom_guest_rs::Continuation) { let other = k.clone(); }
/// ```
///
/// A synchronous Resume/Forward reply disarms the callback's continuation;
/// Deferred requires retaining it, resuming it, or explicitly abandoning it.
pub struct Continuation { state: Arc<ContinuationState> }
impl Continuation {
    pub fn resume<T: Serialize>(self, value: T) -> Result<(), EffectError> {
        let bytes = crate::encode(&value)?;
        if self.state.state.fetch_or(DISARMED, Ordering::AcqRel) & DISARMED != 0 {
            return Err("continuation already resolved".into());
        }
        status(host_resume(self.state.id, &bytes), "continuation resume refused")
    }
    pub fn abandon(self) -> Result<(), EffectError> {
        if self.state.state.fetch_or(DISARMED, Ordering::AcqRel) & DISARMED != 0 {
            return Err("continuation already resolved".into());
        }
        status(host_abandon(self.state.id), "continuation abandon refused")
    }
}
impl Drop for Continuation {
    fn drop(&mut self) {
        let previous = self.state.state.fetch_or(DROPPED, Ordering::AcqRel);
        if previous & (CALLBACK | DISARMED | DROPPED) == 0 {
            if host_drop(self.state.id) != 0 { abort_execution(); }
        }
    }
}
fn finish_callback(state: &ContinuationState, reply: &Reply) {
    match reply {
        Reply::Resume(_) | Reply::Forward => {
            state.state.fetch_or(DISARMED, Ordering::AcqRel);
        }
        Reply::Deferred => {
            let previous = state.state.fetch_and(!CALLBACK, Ordering::AcqRel);
            if previous & DROPPED != 0 && previous & DISARMED == 0 {
                if host_drop(state.id) != 0 { abort_execution(); }
            }
        }
    }
}

struct Handler<H> { callback: UnsafeCell<H>, entered: AtomicBool }
struct Installed<H> { frame: u64, _handler: Box<Handler<H>> }
impl<H> Drop for Installed<H> {
    fn drop(&mut self) {
        // Host pop drains inherited tasks and active callbacks before returning.
        // A failed drain must not run Rust destructors over borrowed storage.
        if host_pop(self.frame) != 0 { abort_execution(); }
    }
}

/// Install a deep handler for the body's dynamic extent. Effects performed
/// by the handler itself dispatch in the outer context, below this frame.
/// Scoped children inherit the frame; calls of another definition do not.
/// The frame is drained before borrowed handler captures can be released.
///
/// ```compile_fail
/// let state = std::rc::Rc::new(1);
/// loom_guest_rs::handle_any(move |_, _| {
///     let _value = *state;
///     loom_guest_rs::Reply::Forward
/// }, || ()).unwrap();
/// ```
pub fn handle_any<H, F, R>(handler: H, body: F) -> Result<R, EffectError>
where H: FnMut(Effect, Continuation) -> Reply + Send, F: FnOnce() -> R {
    install(handler, None, body)
}

/// Install a handler selecting a statically visible set of effect names.
/// An empty set selects no effects. This is also the effect-row checker's
/// effect selection API; `handle_any` selects every effect dynamically.
/// Selected effects must be handled: returning Forward traps the execution.
pub fn handle<'a, H, F, R>(labels: impl AsRef<[&'a str]>, handler: H, body: F) -> Result<R, EffectError>
where H: FnMut(Effect, Continuation) -> Reply + Send, F: FnOnce() -> R {
    install(handler, Some(labels.as_ref()), body)
}
fn install<H, F, R>(handler: H, labels: Option<&[&str]>, body: F) -> Result<R, EffectError>
where H: FnMut(Effect, Continuation) -> Reply + Send, F: FnOnce() -> R {
    let labels = crate::encode(&labels)?;
    let mut handler = Box::new(Handler { callback: UnsafeCell::new(handler), entered: AtomicBool::new(false) });
    let frame = unsafe { host_push(run::<H>, (&mut *handler as *mut Handler<H>).cast(), &labels) };
    if frame == 0 { return Err("handler installation refused".into()); }
    let installed = Installed { frame, _handler: handler };
    let result = body();
    drop(installed);
    Ok(result)
}

unsafe fn run<H: FnMut(Effect, Continuation) -> Reply>(data: *mut (), id: u64, op: Effect) -> Reply {
    // SAFETY: Installed owns the allocation until host pop drains callbacks.
    // Host holds the per-frame asynchronous permit while this callback runs.
    let handler = unsafe { &*data.cast::<Handler<H>>() };
    // Defense against a broken host serialization invariant: never create two
    // mutable references, and never spin while another callback is suspended.
    if handler.entered.swap(true, Ordering::Acquire) { abort_execution(); }
    let state = Arc::new(ContinuationState { id, state: AtomicU8::new(CALLBACK) });
    let continuation = Continuation { state: state.clone() };
    let reply = unsafe { (&mut *handler.callback.get())(op, continuation) };
    // The host rejects Forward from selected-effect frames with a structured
    // error naming the frame. A guest panic would erase that diagnostic.
    finish_callback(&state, &reply);
    handler.entered.store(false, Ordering::Release);
    reply
}

pub(crate) type HandlerRun = unsafe fn(*mut (), u64, Effect) -> Reply;
fn status(code: i32, error: &str) -> Result<(), EffectError> {
    if code == 0 { Ok(()) } else { Err(error.into()) }
}
fn abort_execution() -> ! {
    #[cfg(target_arch = "wasm32")]
    { core::arch::wasm32::unreachable() }
    #[cfg(not(target_arch = "wasm32"))]
    { std::process::abort() }
}
#[cfg(all(loom_core, target_arch = "wasm32"))]
unsafe fn host_push(run: HandlerRun, data: *mut (), labels: &[u8]) -> u64 {
    unsafe { crate::core::host_handle_push(run as usize as u32, data as usize as u32, labels.as_ptr() as u32, labels.len() as u32) }
}
#[cfg(not(all(loom_core, target_arch = "wasm32")))]
unsafe fn host_push(_: HandlerRun, _: *mut (), _: &[u8]) -> u64 { 0 }
#[cfg(all(loom_core, target_arch = "wasm32"))]
fn host_pop(frame: u64) -> i32 { unsafe { crate::core::host_handle_pop(frame) } }
#[cfg(not(all(loom_core, target_arch = "wasm32")))]
fn host_pop(_: u64) -> i32 { -1 }
#[cfg(all(loom_core, target_arch = "wasm32"))]
fn host_resume(id: u64, bytes: &[u8]) -> i32 { unsafe { crate::core::host_resume(id, bytes.as_ptr() as u32, bytes.len() as u32) } }
#[cfg(not(all(loom_core, target_arch = "wasm32")))]
fn host_resume(_: u64, _: &[u8]) -> i32 { -1 }
#[cfg(all(loom_core, target_arch = "wasm32"))]
fn host_abandon(id: u64) -> i32 { unsafe { crate::core::host_abandon(id) } }
#[cfg(not(all(loom_core, target_arch = "wasm32")))]
fn host_abandon(_: u64) -> i32 { -1 }
#[cfg(all(loom_core, target_arch = "wasm32"))]
fn host_drop(id: u64) -> i32 { unsafe { crate::core::host_continuation_drop(id) } }
#[cfg(all(not(test), not(all(loom_core, target_arch = "wasm32"))))]
fn host_drop(_: u64) -> i32 { -1 }
#[cfg(all(test, not(target_arch = "wasm32")))]
static DROPS: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
#[cfg(all(test, not(target_arch = "wasm32")))]
fn host_drop(id: u64) -> i32 { DROPS.lock().unwrap().push(id); 0 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trampoline_mutates_borrowed_capture_and_releases_retained_continuation() {
        let mut observed = Vec::new();
        let mut retained = None;
        {
            let callback = |op: Effect, continuation: Continuation| {
                observed.push(op.name);
                retained = Some(continuation);
                Reply::Deferred
            };
            let mut handler = Box::new(Handler {
                callback: UnsafeCell::new(callback), entered: AtomicBool::new(false),
            });
            fn invoke<H: FnMut(Effect, Continuation) -> Reply>(handler: &mut Handler<H>, id: u64) {
                let pointer = (handler as *mut Handler<H>).cast();
                // SAFETY: the simulated host invokes one callback at a time,
                // retains the allocation, and joins before freeing it.
                let reply = unsafe { run::<H>(pointer, id, Effect { name: "test.tick".into(), args: Value::Null }) };
                assert!(matches!(reply, Reply::Deferred));
            }
            invoke(&mut handler, 3);
            invoke(&mut handler, 4);
            drop(handler);
        }
        assert_eq!(observed, ["test.tick", "test.tick"]);
        assert!(DROPS.lock().unwrap().contains(&3));
        assert!(!DROPS.lock().unwrap().contains(&4));
        drop(retained);
        assert!(DROPS.lock().unwrap().contains(&4));
    }
    #[test]
    fn deferred_drop_and_callback_return_race_reports_exactly_once() {
        for id in 100..200 {
            let state = Arc::new(ContinuationState { id, state: AtomicU8::new(CALLBACK) });
            let continuation = Continuation { state: state.clone() };
            let child = std::thread::spawn(move || drop(continuation));
            finish_callback(&state, &Reply::Deferred);
            child.join().unwrap();
            assert_eq!(DROPS.lock().unwrap().iter().filter(|seen| **seen == id).count(), 1);
        }
    }
    #[test]
    fn deferred_retained_continuation_is_not_prematurely_dropped() {
        let state = Arc::new(ContinuationState { id: 2, state: AtomicU8::new(CALLBACK) });
        let continuation = Continuation { state: state.clone() };
        finish_callback(&state, &Reply::Deferred);
        assert!(!DROPS.lock().unwrap().contains(&2));
        drop(continuation);
        assert_eq!(DROPS.lock().unwrap().iter().filter(|id| **id == 2).count(), 1);
    }
    #[test]
    fn synchronous_reply_disarms_an_implicitly_dropped_continuation() {
        for reply in [Reply::Forward, Reply::Resume(Value::Null)] {
            let state = Arc::new(ContinuationState { id: 1, state: AtomicU8::new(CALLBACK) });
            drop(Continuation { state: state.clone() });
            assert_eq!(state.state.load(Ordering::Acquire), CALLBACK | DROPPED);
            finish_callback(&state, &reply);
            assert_ne!(state.state.load(Ordering::Acquire) & DISARMED, 0);
        }
    }
    #[test]
    fn retained_continuation_is_disarmed_by_inline_reply() {
        let state = Arc::new(ContinuationState { id: 1, state: AtomicU8::new(CALLBACK) });
        let continuation = Continuation { state: state.clone() };
        finish_callback(&state, &Reply::Forward);
        assert_eq!(continuation.resume(7).unwrap_err(), "continuation already resolved");
    }
}
