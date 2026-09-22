//! Trusted shared-memory ABI glue. Host pointers must name live guest allocations.
use crate::{CallError, EffectError};
use serde::Serialize;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "loom")]
unsafe extern "C" {
    #[link_name = "handle_push"]
    pub(crate) fn host_handle_push(
        function: u32,
        data: u32,
        labels_ptr: u32,
        labels_len: u32,
    ) -> u64;
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
    #[link_name = "call"]
    fn host_call(pointer: u32, length: u32) -> u64;
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
    {
        let _ = bytes;
        Err("core effects require wasm32".into())
    }
}

/// Hand an isolated-call request frame to the host and take ownership of the
/// response frame it allocated through `loom_alloc(length, 1)`. The frame is
/// parsed by `crate::isolated::call`; this function moves bytes only.
pub(crate) fn isolated(frame: &[u8], hash: &str) -> Result<Vec<u8>, CallError> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = hash;
        // SAFETY: the frame stays live across host suspension; the host returns
        // an allocation made through loom_alloc(length, 1), transferring ownership.
        let packed = unsafe { host_call(frame.as_ptr() as u32, frame.len() as u32) };
        let pointer = packed as u32 as *mut u8;
        let length = (packed >> 32) as usize;
        Ok(unsafe { Vec::from_raw_parts(pointer, length, length) })
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = frame;
        Err(CallError::Trapped {
            hash: hash.to_owned(),
            message: "isolated calls require wasm32".into(),
        })
    }
}

/// Pack a callee's response frame for the host: `[0][result]` or
/// `[1][CallError]`. Used by the generated `loom_call_<entry>` wrappers.
pub fn isolated_response(result: Result<Vec<u8>, CallError>) -> u64 {
    let frame = loom_proto::isolated::response_frame(match &result {
        Ok(payload) => Ok(payload.as_slice()),
        Err(error) => Err(error),
    })
    .into_boxed_slice();
    let length = frame.len() as u64;
    let pointer = Box::into_raw(frame) as *mut u8 as u32;
    (length << 32) | pointer as u64
}

#[cfg(any(target_arch = "wasm32", test))]
struct HostResponse<T> {
    result: Result<T, String>,
}
#[cfg(any(target_arch = "wasm32", test))]
impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for HostResponse<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T>(std::marker::PhantomData<T>);
        impl<'de, T: serde::Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
            type Value = HostResponse<T>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("one host response result")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let key: String = map
                    .next_key()?
                    .ok_or_else(|| serde::de::Error::custom("missing host response"))?;
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
    if length == 0 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(pointer as *const u8, length as usize) }
}

#[derive(Serialize)]
#[serde(untagged)]
enum Response<T> {
    Success { ok: T },
    Failure { error: String },
}

pub fn response<T: Serialize>(result: Result<T, String>) -> u64 {
    let response = match result {
        Ok(ok) => Response::Success { ok },
        Err(error) => Response::Failure { error },
    };
    let bytes = crate::encode(&response)
        .expect("invalid core response")
        .into_boxed_slice();
    let length = bytes.len() as u64;
    let pointer = Box::into_raw(bytes) as *mut u8 as u32;
    (length << 32) | pointer as u64
}

#[unsafe(no_mangle)]
pub extern "C" fn loom_alloc(size: u32, align: u32) -> u32 {
    let Ok(layout) = std::alloc::Layout::from_size_align(size.max(1) as usize, align as usize)
    else {
        return 0;
    };
    // SAFETY: valid nonzero layout; host checks allocation failure.
    unsafe { std::alloc::alloc(layout) as u32 }
}

/// # Safety
/// Pointer and layout must match a live allocation returned by this guest.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn loom_dealloc(pointer: u32, size: u32, align: u32) {
    let layout = std::alloc::Layout::from_size_align(size.max(1) as usize, align as usize)
        .expect("invalid allocation layout");
    unsafe {
        std::alloc::dealloc(pointer as *mut u8, layout);
    }
}

/// # Safety
/// Host invokes each registered function/data pair exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn loom_task_run(function: u32, data: u32) {
    let run: unsafe fn(*mut ()) = unsafe { std::mem::transmute(function as usize) };
    unsafe {
        run(data as *mut ());
    }
}

#[cfg(target_arch = "wasm32")]
mod allocator {
    use std::{
        alloc::{GlobalAlloc, Layout},
        cell::UnsafeCell,
        sync::atomic::{AtomicBool, Ordering},
    };
    struct SharedAllocator {
        locked: AtomicBool,
        heap: UnsafeCell<dlmalloc::Dlmalloc>,
    }
    // SAFETY: all allocator state access is serialized by locked. Critical
    // sections cannot invoke guest effects or suspend a Store.
    unsafe impl Sync for SharedAllocator {}
    impl SharedAllocator {
        fn with_heap<T>(&self, f: impl FnOnce(&mut dlmalloc::Dlmalloc) -> T) -> T {
            while self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
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
    static ALLOCATOR: SharedAllocator = SharedAllocator {
        locked: AtomicBool::new(false),
        heap: UnsafeCell::new(dlmalloc::Dlmalloc::new()),
    };
}

/// # Safety
/// Host supplies an installed frame, serializes its callbacks, and retains the
/// immutable op allocation until return. Returned bytes transfer to the host,
/// which frees them with loom_dealloc(pointer, length, 1).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn loom_handler_run(
    function: u32,
    data: u32,
    k: u64,
    op_ptr: u32,
    op_len: u32,
) -> u64 {
    let op = crate::decode_host(unsafe { input(op_ptr, op_len) }).expect("invalid handler effect");
    let run: crate::handlers::HandlerRun = unsafe { std::mem::transmute(function as usize) };
    let reply = unsafe { run(data as *mut (), k, op) };
    #[derive(Serialize)]
    #[serde(untagged)]
    enum WireReply {
        Resume { resume: crate::Value },
        Forward { forward: () },
        Deferred { deferred: () },
    }
    let wire = match reply {
        crate::Reply::Resume(resume) => WireReply::Resume { resume },
        crate::Reply::Forward => WireReply::Forward { forward: () },
        crate::Reply::Deferred => WireReply::Deferred { deferred: () },
    };
    let bytes = crate::encode(&wire)
        .expect("invalid handler reply")
        .into_boxed_slice();
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
    {
        unsafe { host_perform(pointer, length) }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = pointer;
        let _ = length;
        response::<()>(Err("core effects require wasm32".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_response_envelopes_decode_typed_results_and_errors() {
        let entries = vec![crate::DirEntry {
            name: "file.rs".into(),
            size: 42,
            kind: crate::EntryKind::File,
        }];
        let typed = crate::encode(&Response::Success { ok: &entries }).unwrap();
        let decoded: HostResponse<Vec<crate::DirEntry>> = crate::decode_host(&typed).unwrap();
        assert_eq!(decoded.result.unwrap(), entries);
        let failed = crate::encode(&Response::<()>::Failure {
            error: "task failed".into(),
        })
        .unwrap();
        let decoded: HostResponse<()> = crate::decode_host(&failed).unwrap();
        assert_eq!(decoded.result, Err("task failed".into()));
    }

    #[test]
    fn isolated_response_frames_are_tagged_and_owned() {
        let payload = loom_proto::isolated::encode_payload(&7u8).unwrap();
        let packed = isolated_response(Ok(payload.clone()));
        let (pointer, length) = (packed as u32 as *mut u8, (packed >> 32) as usize);
        // SAFETY: the test owns the leaked frame and frees it once.
        let frame = unsafe { Vec::from_raw_parts(pointer, length, length) };
        assert_eq!(frame[0], 0);
        assert_eq!(&frame[1..], payload);
        let packed = isolated_response(Err(CallError::Decode {
            message: "bad".into(),
        }));
        let (pointer, length) = (packed as u32 as *mut u8, (packed >> 32) as usize);
        let frame = unsafe { Vec::from_raw_parts(pointer, length, length) };
        assert_eq!(frame[0], 1);
        assert_eq!(
            loom_proto::isolated::Response::parse(&frame)
                .unwrap()
                .unwrap_err(),
            CallError::Decode {
                message: "bad".into()
            }
        );
    }
}
