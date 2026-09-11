//! Trusted shared-memory ABI glue. Host pointers must name live guest allocations.
use crate::EffectError;
use serde::Serialize;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "loom")]
unsafe extern "C" {
    #[link_name = "handle_push"]
    pub(crate) fn host_handle_push(function: u32, data: u32, labels_ptr: u32, labels_len: u32) -> u64;
    #[link_name = "handle_pop"]
    pub(crate) fn host_handle_pop(frame: u64) -> i32;
    #[link_name = "resume"]
    pub(crate) fn host_resume(k: u64, pointer: u32, length: u32) -> i32;
    #[link_name = "abandon"]
    pub(crate) fn host_abandon(k: u64) -> i32;
    #[link_name = "continuation_drop"]
    pub(crate) fn host_continuation_drop(k: u64) -> i32;
    #[link_name = "perform"]
    fn host_perform(pointer: u32, length: u32) -> u64;
    #[link_name = "spawn"]
    pub(crate) fn host_spawn(function: u32, data: u32, detached: i32) -> u64;
    #[link_name = "join"]
    pub(crate) fn host_join(id: u64) -> i32;
    #[link_name = "join_error"]
    pub(crate) fn host_join_error(id: u64) -> u64;
}

pub fn perform<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, EffectError> {
    #[cfg(target_arch = "wasm32")]
    {
        // SAFETY: input remains live across host suspension; host returns an
        // allocation made through loom_alloc(length, 1), transferring ownership.
        let packed = unsafe { host_perform(bytes.as_ptr() as u32, bytes.len() as u32) };
        let pointer = packed as u32 as *mut u8;
        let length = (packed >> 32) as usize;
        let bytes = unsafe { Vec::from_raw_parts(pointer, length, length) };
        let response: HostResponse<T> = crate::decode_host(&bytes)?;
        response.result
    }
    #[cfg(not(target_arch = "wasm32"))]
    { let _ = bytes; Err("core effects require wasm32".into()) }
}

#[cfg(any(target_arch = "wasm32", test))]
struct HostResponse<T> { result: Result<T, String> }
#[cfg(any(target_arch = "wasm32", test))]
impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for HostResponse<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T>(std::marker::PhantomData<T>);
        impl<'de, T: serde::Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
            type Value = HostResponse<T>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("one host response result")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let key: String = map.next_key()?.ok_or_else(|| serde::de::Error::custom("missing host response"))?;
                let result = match key.as_str() {
                    "ok" => Ok(map.next_value()?),
                    "error" => Err(map.next_value()?),
                    _ => return Err(serde::de::Error::custom("unknown host response field")),
                };
                if map.next_key::<String>()?.is_some() {
                    return Err(serde::de::Error::custom("multiple host response results"));
                }
                Ok(HostResponse { result })
            }
        }
        deserializer.deserialize_map(Visitor(std::marker::PhantomData))
    }
}

/// # Safety
/// The host provides a valid immutable guest allocation for this call's duration.
pub unsafe fn input<'a>(pointer: u32, length: u32) -> &'a [u8] {
    if length == 0 { return &[]; }
    unsafe { std::slice::from_raw_parts(pointer as *const u8, length as usize) }
}

#[derive(Serialize)]
#[serde(untagged)]
enum Response<T> { Success { ok: T }, Failure { error: String } }

pub fn response<T: Serialize>(result: Result<T, String>) -> u64 {
    let response = match result {
        Ok(ok) => Response::Success { ok },
        Err(error) => Response::Failure { error },
    };
    let bytes = crate::encode(&response).expect("invalid core response").into_boxed_slice();
    let length = bytes.len() as u64;
    let pointer = Box::into_raw(bytes) as *mut u8 as u32;
    (length << 32) | pointer as u64
}

#[unsafe(no_mangle)]
pub extern "C" fn loom_alloc(size: u32, align: u32) -> u32 {
    let Ok(layout) = std::alloc::Layout::from_size_align(size.max(1) as usize, align as usize) else { return 0; };
    // SAFETY: valid nonzero layout; host checks allocation failure.
    unsafe { std::alloc::alloc(layout) as u32 }
}

/// # Safety
/// Pointer and layout must match a live allocation returned by this guest.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn loom_dealloc(pointer: u32, size: u32, align: u32) {
    let layout = std::alloc::Layout::from_size_align(size.max(1) as usize, align as usize).expect("invalid allocation layout");
    unsafe { std::alloc::dealloc(pointer as *mut u8, layout); }
}

/// # Safety
/// Host invokes each registered function/data pair exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn loom_task_run(function: u32, data: u32) {
    let run: unsafe fn(*mut ()) = unsafe { std::mem::transmute(function as usize) };
    unsafe { run(data as *mut ()); }
}

#[cfg(target_arch = "wasm32")]
mod allocator {
    use std::{alloc::{GlobalAlloc, Layout}, cell::UnsafeCell, sync::atomic::{AtomicBool, Ordering}};
    struct SharedAllocator { locked: AtomicBool, heap: UnsafeCell<dlmalloc::Dlmalloc> }
    // SAFETY: all allocator state access is serialized by locked. Critical
    // sections cannot invoke guest effects or suspend a Store.
    unsafe impl Sync for SharedAllocator {}
    impl SharedAllocator {
        fn with_heap<T>(&self, f: impl FnOnce(&mut dlmalloc::Dlmalloc) -> T) -> T {
            while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
                std::hint::spin_loop();
            }
            let result = f(unsafe { &mut *self.heap.get() });
            self.locked.store(false, Ordering::Release);
            result
        }
    }
    unsafe impl GlobalAlloc for SharedAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            self.with_heap(|heap| unsafe { heap.malloc(layout.size(), layout.align()) })
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            self.with_heap(|heap| unsafe { heap.free(pointer, layout.size(), layout.align()) });
        }
    }
    #[global_allocator]
    static ALLOCATOR: SharedAllocator = SharedAllocator { locked: AtomicBool::new(false), heap: UnsafeCell::new(dlmalloc::Dlmalloc::new()) };
}

pub trait Guest {
    fn init() -> Result<Vec<u8>, String> { Err("free definition has no actor initializer".into()) }
    fn call(definition: Vec<u8>, args: Vec<u8>) -> Result<Vec<u8>, String>;
    fn run(state: Vec<u8>, message: Vec<u8>) -> Result<Vec<u8>, String>;
    fn fold(state: Vec<u8>, event: Vec<u8>) -> Vec<u8>;
}

pub fn encoded_response(result: Result<Vec<u8>, String>) -> u64 {
    let bytes = encoded_response_bytes(result).into_boxed_slice();
    let length = bytes.len() as u64;
    let pointer = Box::into_raw(bytes) as *mut u8 as u32;
    (length << 32) | pointer as u64
}

fn encoded_response_bytes(result: Result<Vec<u8>, String>) -> Vec<u8> {
    match result {
        Ok(bytes) => {
            // The generated Guest methods already encode canonical CBOR. Wrap
            // that owned result without decoding it into a second value tree.
            let mut envelope = Vec::with_capacity(4 + bytes.len());
            envelope.extend_from_slice(b"\xa1\x62ok");
            envelope.extend_from_slice(&bytes);
            envelope
        }
        Err(error) => crate::encode(&Response::<()>::Failure { error })
            .expect("invalid core response"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_envelopes_match_typed_arrays_null_and_errors() {
        let entries = vec![crate::DirEntry {
            name: "file.rs".into(), size: 42, kind: crate::EntryKind::File,
        }];
        let typed = encoded_response_bytes(Ok(crate::encode(&entries).unwrap()));
        assert_eq!(typed, crate::encode(&Response::Success { ok: &entries }).unwrap());
        let decoded: HostResponse<Vec<crate::DirEntry>> = crate::decode_host(&typed).unwrap();
        assert_eq!(decoded.result.unwrap(), entries);

        let null = encoded_response_bytes(Ok(crate::encode(&()).unwrap()));
        assert_eq!(null, crate::encode(&Response::Success { ok: () }).unwrap());
        let decoded: HostResponse<()> = crate::decode_host(&null).unwrap();
        assert_eq!(decoded.result, Ok(()));

        let error = "task failed".to_string();
        let failed = encoded_response_bytes(Err(error.clone()));
        assert_eq!(failed, crate::encode(&Response::<()>::Failure { error: error.clone() }).unwrap());
        let decoded: HostResponse<()> = crate::decode_host(&failed).unwrap();
        assert_eq!(decoded.result, Err(error));
    }

    #[test]
    fn malformed_guest_payloads_remain_for_strict_host_admission() {
        for payload in [vec![0, 0], vec![0x18, 0]] {
            let wrapped = encoded_response_bytes(Ok(payload.clone()));
            assert_eq!(&wrapped[4..], payload);
            assert!(crate::decode::<crate::Value>(&wrapped).is_err());
        }
    }
}

#[macro_export]
macro_rules! export_core {
    ($guest:ty) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn loom_call(pointer: u32, length: u32) -> u64 {
            let args = unsafe { $crate::core::input(pointer, length) }.to_vec();
            $crate::core::encoded_response(<$guest as $crate::core::Guest>::call(Vec::new(), args))
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn loom_init() -> u64 {
            $crate::core::encoded_response(<$guest as $crate::core::Guest>::init())
        }
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn loom_run(state: u32, state_len: u32, msg: u32, msg_len: u32) -> u64 {
            let state = unsafe { $crate::core::input(state, state_len) }.to_vec();
            let msg = unsafe { $crate::core::input(msg, msg_len) }.to_vec();
            $crate::core::encoded_response(<$guest as $crate::core::Guest>::run(state, msg))
        }
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn loom_fold(state: u32, state_len: u32, event: u32, event_len: u32) -> u64 {
            let state = unsafe { $crate::core::input(state, state_len) }.to_vec();
            let event = unsafe { $crate::core::input(event, event_len) }.to_vec();
            $crate::core::encoded_response(Ok(<$guest as $crate::core::Guest>::fold(state, event)))
        }
    };
}

/// # Safety
/// Host supplies an installed frame, serializes its callbacks, and retains the
/// immutable op allocation until return. Returned bytes transfer to the host,
/// which frees them with loom_dealloc(pointer, length, 1).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn loom_handler_run(function: u32, data: u32, k: u64, op_ptr: u32, op_len: u32) -> u64 {
    let op = crate::decode_host(unsafe { input(op_ptr, op_len) }).expect("invalid handler effect");
    let run: crate::handlers::HandlerRun = unsafe { std::mem::transmute(function as usize) };
    let reply = unsafe { run(data as *mut (), k, op) };
    #[derive(Serialize)]
    #[serde(untagged)]
    enum WireReply { Resume { resume: crate::Value }, Forward { forward: () }, Deferred { deferred: () } }
    let wire = match reply {
        crate::Reply::Resume(resume) => WireReply::Resume { resume },
        crate::Reply::Forward => WireReply::Forward { forward: () },
        crate::Reply::Deferred => WireReply::Deferred { deferred: () },
    };
    let bytes = crate::encode(&wire).expect("invalid handler reply").into_boxed_slice();
    let length = bytes.len() as u64;
    let pointer = Box::into_raw(bytes) as *mut u8 as u32;
    (length << 32) | pointer as u64
}

/// Enter ordinary effect dispatch from a host-scheduled child instance. This
/// bridge deliberately preserves the effect and response bytes: compound
/// effects use exactly the same handler stack and canonical admission as a
/// direct guest perform, without a second codec round trip.
///
/// # Safety
/// Input names a live immutable allocation retained by the host until return.
/// The host owns the returned response allocation and must release it through
/// loom_dealloc(pointer, length, 1). This function does not consume the input.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn loom_effect_run(pointer: u32, length: u32) -> u64 {
    #[cfg(target_arch = "wasm32")]
    { unsafe { host_perform(pointer, length) } }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = pointer;
        let _ = length;
        response::<()>(Err("core effects require wasm32".into()))
    }
}
