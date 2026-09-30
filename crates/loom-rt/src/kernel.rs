//! Host kernels: native code a wasm guest calls by name, registered by the embedder.
//!
//! A kernel is a pure function over bytes that is too heavy or too native to run
//! in the guest: a BVH query over a million triangles, a physics query. Loom does
//! not know any: the program that embeds `loom-rt` registers them
//! ([`Runtime::register_kernel`]), so this crate depends on none of their code.
//!
//! **Handles are content hashes.** [`Runtime::call_kernel`] with `loom.put` stores
//! a buffer in the content-addressed store once and returns its 32-byte BLAKE3
//! hash. A kernel op takes that hash instead of the bytes, so a mesh crosses the
//! guest boundary once and every later query sends 32 bytes. A hash is a value: it
//! can be returned, stored, and cached, it means the same on every host, and an
//! unknown one is an error, not a pointer. What a kernel derives from a handle (a
//! built BVH) is the kernel's own cache, keyed by the hash.
//!
//! **Pure ops only.** An op does nothing observable but compute: it records no
//! trace entry, so it does not make a callee ineligible for the result cache
//! (`result_cache.rs`). The kernels' versions are part of that cache's key
//! ([`Runtime::kernel_fingerprint`]), so fixing or changing a kernel invalidates
//! every result that could have used it. Effectful ops (a stateful physics world)
//! are not supported yet and are refused at registration.
use super::*;
use arc_swap::ArcSwap;
use std::{
    collections::{BTreeMap, HashMap},
    panic::AssertUnwindSafe,
    sync::{Mutex, atomic::AtomicU64},
};

/// The store kind `loom.put` writes and `KernelContext::blob` reads.
const BLOB_KIND: &str = "kernel-blob";

/// A content handle: the BLAKE3 hash of the bytes it names.
pub type Handle = [u8; 32];

/// A family of native ops, registered as a unit and versioned as a unit.
pub trait HostKernel: Send + Sync {
    /// The op prefix: op `ray` of family `rgb-host` is called `rgb-host.ray`.
    /// `loom` is reserved.
    fn family(&self) -> &str;
    /// Bumped whenever an op's result for the same input could change.
    fn version(&self) -> u32;
    /// The ops this kernel answers. All are pure.
    fn ops(&self) -> &[&'static str];
    /// Run `op` (without the family prefix) on `args`, the guest's gather list of
    /// buffers, in order. `Err` is delivered to the guest as a kernel error.
    fn call(
        &self,
        context: &KernelContext<'_>,
        op: &str,
        args: &[&[u8]],
    ) -> Result<Vec<u8>, String>;
}

/// What a kernel may use of the runtime while it runs.
pub struct KernelContext<'a> {
    store: &'a Store,
}

impl KernelContext<'_> {
    /// The bytes a handle names (`loom.put`), or `None` when this host never
    /// stored them through `loom.put`. A hash of anything else in the store (a
    /// definition, a component) is `None` too, so a handle cannot read what the
    /// guest could not already have written.
    pub fn blob(&self, handle: &Handle) -> Result<Option<Vec<u8>>, String> {
        self.store
            .get_of_kind(&hex(handle), BLOB_KIND)
            .map_err(|error| format!("reading kernel blob: {error:#}"))
    }
}

impl KernelContext<'_> {
    /// [`Self::blob`] without the copy: a file-backed blob (1 MiB and up) is a read-only mapping of the
    /// store's object file, verified once, so a kernel reading a large mesh touches the pages in place.
    pub fn map(&self, handle: &Handle) -> Result<Option<loom_store::MappedObject>, String> {
        self.store
            .map_object_of_kind(&hex(handle), BLOB_KIND)
            .map_err(|error| format!("mapping kernel blob: {error:#}"))
    }
    /// Store `bytes` the way `loom.put` does and return the handle. A kernel returns a handle (32 bytes)
    /// instead of a large result; the caller reads it with [`Runtime::map_blob`] or hands it to the next op.
    pub fn put(&self, bytes: &[u8]) -> Result<Handle, String> {
        let stored = self
            .store
            .put(BLOB_KIND, bytes)
            .map_err(|error| format!("storing kernel blob: {error:#}"))?;
        unhex(&stored).ok_or_else(|| "the store returned a malformed hash".into())
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn unhex(text: &str) -> Option<Handle> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, pair) in bytes.chunks(2).enumerate() {
        let digit = |c: u8| (c as char).to_digit(16);
        out[index] = (digit(pair[0])? * 16 + digit(pair[1])?) as u8;
    }
    Some(out)
}

/// The registry is an immutable snapshot behind an `ArcSwap`: every kernel call
/// reads it, registration (once, at startup) replaces it. A read is one atomic
/// load and no lock, so guests on many threads do not contend on a reader count.
#[derive(Default)]
struct Snapshot {
    ops: HashMap<String, Arc<dyn HostKernel>>,
    versions: BTreeMap<String, u32>,
    fingerprint: [u8; 32],
}

pub(crate) struct Kernels {
    snapshot: ArcSwap<Snapshot>,
    /// Serializes registrations; readers never take it.
    registering: Mutex<()>,
    /// Kernel calls running at once. They run off the guest thread pool (a slow one
    /// must not stall every guest), and this bounds how many native threads and how
    /// much transient memory they take together.
    slots: Arc<tokio::sync::Semaphore>,
    /// Kernel calls that failed, ever. A kernel failure depends on host state (a
    /// missing blob, a denied permit) and is recorded nowhere else, so a caller that
    /// saw this move during a call does not store that call's result.
    failures: AtomicU64,
}

impl Default for Kernels {
    fn default() -> Self {
        let slots = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self {
            snapshot: ArcSwap::default(),
            registering: Mutex::new(()),
            slots: Arc::new(tokio::sync::Semaphore::new(slots)),
            failures: AtomicU64::new(0),
        }
    }
}

impl Kernels {
    pub(crate) fn register(&self, kernel: Arc<dyn HostKernel>) -> Result<()> {
        let family = kernel.family().to_owned();
        anyhow::ensure!(
            !family.is_empty() && family != "loom" && !family.contains('.'),
            "kernel family {family:?} is reserved or malformed"
        );
        let _one_at_a_time = self
            .registering
            .lock()
            .expect("kernel registration poisoned");
        let current = self.snapshot.load_full();
        anyhow::ensure!(
            !current.versions.contains_key(&family),
            "kernel family {family:?} is already registered"
        );
        let mut ops = current.ops.clone();
        for op in kernel.ops() {
            ops.insert(format!("{family}.{op}"), kernel.clone());
        }
        let mut versions = current.versions.clone();
        versions.insert(family, kernel.version());
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"loom-kernels-v1");
        for (family, version) in &versions {
            hasher.update(&(family.len() as u64).to_le_bytes());
            hasher.update(family.as_bytes());
            hasher.update(&version.to_le_bytes());
        }
        let fingerprint = *hasher.finalize().as_bytes();
        self.snapshot.store(Arc::new(Snapshot {
            ops,
            versions,
            fingerprint,
        }));
        Ok(())
    }

    /// A hash of every registered family and its version, computed at registration.
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        self.snapshot.load().fingerprint
    }

    fn op(&self, name: &str) -> Option<Arc<dyn HostKernel>> {
        self.snapshot.load().ops.get(name).cloned()
    }
}

impl Runtime {
    /// Make a family of native ops callable from guests. Refused when the family
    /// name is reserved or already taken.
    pub fn register_kernel(&self, kernel: Arc<dyn HostKernel>) -> Result<()> {
        self.inner.kernels.register(kernel)
    }

    /// A hash of the registered kernel families and versions; part of the result
    /// cache key.
    pub fn kernel_fingerprint(&self) -> [u8; 32] {
        self.inner.kernels.fingerprint()
    }

    /// How many kernel calls have failed on this runtime.
    pub(crate) fn kernel_failures(&self) -> u64 {
        self.inner
            .kernels
            .failures
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Note a failure that happened before a kernel ran (a denied permit, a bad call).
    pub(crate) fn note_kernel_failure(&self) {
        self.inner
            .kernels
            .failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// [`Self::call_kernel`] on a blocking thread, at most one per core at a time.
    /// This is what the `loom.kernel` import uses: a kernel call cannot be interrupted,
    /// so it must not run on (or hold) a guest executor thread.
    pub(crate) async fn call_kernel_blocking(
        &self,
        op: String,
        args: Vec<Vec<u8>>,
    ) -> Result<Vec<u8>, String> {
        let slot = self
            .inner
            .kernels
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "kernel slots closed".to_owned())?;
        let runtime = self.clone();
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let slices: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
            runtime.call_kernel(&op, &slices)
        })
        .await
        .map_err(|error| format!("kernel task failed: {error}"))?
    }

    /// The bytes a kernel handle names, as memory (see [`KernelContext::map`]). For the embedder: the
    /// engine maps a large result in place instead of copying it out of a reply.
    pub fn map_blob(&self, handle: &Handle) -> Result<Option<loom_store::MappedObject>> {
        self.inner.store.map_object_of_kind(&hex(handle), BLOB_KIND)
    }

    /// [`Self::map_blob`] for a [`loom_proto::StoreRef`] a cell or kernel returned: the object's real length
    /// must equal the reference's, since a cell wrote the number.
    pub fn map_ref(&self, reference: &loom_proto::StoreRef) -> Result<Option<loom_store::MappedObject>> {
        self.inner.store.map_store_ref(reference, Some(BLOB_KIND))
    }

    /// Run kernel op `op` (`family.name`, or the built-in `loom.put`) on the
    /// gather list `args`. This is what the `loom.kernel` import does.
    pub fn call_kernel(&self, op: &str, args: &[&[u8]]) -> Result<Vec<u8>, String> {
        let outcome = self.call_kernel_inner(op, args);
        if outcome.is_err() {
            self.note_kernel_failure();
        }
        outcome
    }

    fn call_kernel_inner(&self, op: &str, args: &[&[u8]]) -> Result<Vec<u8>, String> {
        if op == "loom.put" {
            let joined;
            let bytes: &[u8] = match args {
                [single] => single,
                _ => {
                    joined = args.concat();
                    &joined
                }
            };
            let stored = self
                .inner
                .store
                .put(BLOB_KIND, bytes)
                .map_err(|error| format!("storing kernel blob: {error:#}"))?;
            return unhex(&stored)
                .map(|handle| handle.to_vec())
                .ok_or_else(|| "the store returned a malformed hash".into());
        }
        let kernel = self
            .inner
            .kernels
            .op(op)
            .ok_or_else(|| format!("no kernel op {op:?}"))?;
        let name = op.split_once('.').map_or(op, |(_, name)| name);
        let context = KernelContext {
            store: &self.inner.store,
        };
        std::panic::catch_unwind(AssertUnwindSafe(|| kernel.call(&context, name, args)))
            .unwrap_or_else(|_| Err(format!("kernel op {op:?} panicked")))
    }
}

#[cfg(test)]
mod tests;
