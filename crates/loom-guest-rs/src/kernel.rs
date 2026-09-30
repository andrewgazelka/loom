//! Native kernels a host registered (`loom-rt` `kernel.rs`): pure functions over
//! bytes, called by name with a gather list of buffers.
//!
//! A [`Handle`] is the BLAKE3 hash of stored bytes. [`put`] stores a buffer on the
//! host once and returns its handle; a kernel op then takes the 32-byte handle
//! instead of the data, so a large mesh crosses the boundary once and every query
//! sends a few small buffers. Buffers are raw bytes: no codec runs on this path.
//!
//! Every call is an effect labelled `kernel` in a definition's row, which is what
//! lets the result cache (which keys on the host's kernel versions) serve a callee
//! that only calls pure kernel ops.
use core::fmt;

/// A content handle: the BLAKE3 hash of the bytes it names.
pub type Handle = [u8; 32];

/// A kernel call that failed: an unknown op, a host without kernels, or the
/// kernel's own error, with its message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelError(pub String);

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for KernelError {}

/// Run host op `op` (`family.name`) on `args`, read in order, and take the result
/// bytes.
pub fn call(op: &str, args: &[&[u8]]) -> Result<Vec<u8>, KernelError> {
    #[cfg(target_arch = "wasm32")]
    {
        // The gather list: one (pointer, length) pair per buffer.
        let table: Vec<u32> = args
            .iter()
            .flat_map(|part| [part.as_ptr() as u32, part.len() as u32])
            .collect();
        // SAFETY: the buffers and the table stay live across the call. The host
        // returns one allocation made through loom_alloc(length, 1), the result
        // bytes followed by a tag byte, and transfers its ownership.
        let packed = unsafe {
            crate::core::host_kernel(
                op.as_ptr() as u32,
                op.len() as u32,
                table.as_ptr() as u32,
                args.len() as u32,
            )
        };
        let pointer = packed as u32 as *mut u8;
        let length = (packed >> 32) as usize;
        let mut reply = unsafe { Vec::from_raw_parts(pointer, length, length) };
        match reply.pop() {
            Some(0) => Ok(reply),
            Some(_) => Err(KernelError(String::from_utf8_lossy(&reply).into_owned())),
            None => Err(KernelError("empty kernel reply".into())),
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (op, args);
        Err(KernelError("host kernels require wasm32".into()))
    }
}

/// The bytes a handle names, copied into this guest's memory once. The handle must have been stored through
/// [`put`] (by this guest or by the host for it); anything else is a [`KernelError`].
pub fn get(handle: &Handle) -> Result<Vec<u8>, KernelError> {
    call("loom.get", &[handle])
}

/// Store `parts`, joined, on the host and take their handle.
pub fn put(parts: &[&[u8]]) -> Result<Handle, KernelError> {
    let reply = call("loom.put", parts)?;
    reply
        .try_into()
        .map_err(|_| KernelError("the host returned a handle that is not 32 bytes".into()))
}
