//! Shared linear memory belongs to one execution. Stores never borrow host slices
//! of that memory, and aborted guest stacks remain allocated until every task ends.
use super::*;
use futures::{
    FutureExt,
    future::{AbortHandle, Abortable, RemoteHandle},
    task::SpawnExt,
};
use std::sync::atomic::AtomicBool;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use wasmtime::{
    Caller, Config, ExternType, Instance as CoreInstance, Module, SharedMemory, Strategy,
    UpdateDeadline,
};

mod cancellation;
mod dispatch;
mod execution;
mod linker;
use linker::linker;
mod handlers;
mod timing;
use handlers::{ContinuationState, HandlerFrame, HandlerInstance};

const MAX_JOBS: usize = 512;
const STACK_BYTES: u32 = 256 * 1024;
const MAX_MEMORY: u64 = 256 * 1024 * 1024;
const EXECUTION_SECONDS: u64 = 30;

/// The cache is namespaced by the engine it serves, so it is built from the same
/// configuration before that configuration installs it.
pub(super) fn engine(store: Store) -> Result<(Engine, Arc<LoomCompilationCache>)> {
    let mut config = Config::new();
    config
        .strategy(Strategy::Cranelift)
        .wasm_threads(true)
        .shared_memory(true)
        .epoch_interruption(true);
    // Wasm leaves the payload bits of a NaN unspecified, so two hosts (or two
    // Cranelift versions) may return different bytes for a computation that
    // produces one. This makes every float operation write the canonical NaN, at a
    // cost measured in the rgb spike (docs/spikes/rgb-forge.md). Off by default; the
    // setting is part of the compilation cache namespace, so builds never mix.
    if std::env::var_os("LOOM_WASM_NAN_CANONICALIZATION").is_some_and(|value| value != "0") {
        config.cranelift_nan_canonicalization(true);
    }
    let cache = Arc::new(LoomCompilationCache::new(store, &config)?);
    config.enable_incremental_compilation(cache.clone())?;
    let engine = crate::wasm_engine::create(&config)?;
    Ok((engine, cache))
}
// Keep the classification while sharing a failure with cancelled sibling tasks.
#[derive(Clone)]
struct ExecutionFailure {
    message: String,
    guest: bool,
}
impl ExecutionFailure {
    fn new(error: anyhow::Error) -> Self {
        Self {
            guest: error.is::<GuestFailure>(),
            message: format!("{error:#}"),
        }
    }
    fn into_error(self) -> anyhow::Error {
        if self.guest {
            GuestFailure::new(self.message).into()
        } else {
            anyhow::anyhow!(self.message)
        }
    }
}
struct Job {
    handlers: Vec<u64>,
    parent: String,
    detached: bool,
    result: Mutex<Option<std::result::Result<(), ExecutionFailure>>>,
    done: Notify,
}
struct ScheduledTask {
    abort: AbortHandle,
    completion: RemoteHandle<()>,
}
struct Execution {
    handler_instances: Mutex<Vec<HandlerInstance>>,
    handler_instance_reuses: AtomicU64,
    handler_round_trip_us: Mutex<Vec<f64>>,
    handlers_next: AtomicU64,
    continuations: Mutex<HashMap<u64, Arc<ContinuationState>>>,
    handler_failure: Mutex<Option<ExecutionFailure>>,
    runtime: Runtime,
    module: Module,
    memory: SharedMemory,
    effects: EffectContext,
    /// Set when this execution runs as a stream; see `EffectContext::stream`.
    stream: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    pure: bool,
    jobs: Mutex<HashMap<u64, Arc<Job>>>,
    tasks: Mutex<Vec<ScheduledTask>>,
    job_count: AtomicU64,
    initialization: AsyncMutex<()>,
    permits: Arc<Semaphore>,
    cancelled: AtomicBool,
    cancellation: Notify,
    deadline: Instant,
}
struct Guest {
    execution: Arc<Execution>,
    scope: String,
    handlers: Vec<Arc<HandlerFrame>>,
    occurrence: i64,
    last_effect_error: Option<String>,
    permit: Option<OwnedSemaphorePermit>,
}
struct Allocation {
    stack: u32,
    tls: u32,
}
struct Running {
    store: wasmtime::Store<Guest>,
    instance: CoreInstance,
}
struct Cleanup {
    execution: Option<Arc<Execution>>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let Some(execution) = self.execution.take() else {
            return;
        };
        execution.cancel();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                execution.drain().await;
            });
        }
    }
}
impl Execution {
    fn original_failure(&self) -> Option<ExecutionFailure> {
        self.handler_failure.lock().unwrap().clone().or_else(|| {
            self.jobs.lock().unwrap().values().find_map(|job| {
                if job.detached {
                    return None;
                }
                job.result
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|result| result.as_ref().err())
                    .cloned()
            })
        })
    }
}
enum Invocation {
    /// `args` is the opaque isolated-call payload; the host copies it in as is.
    Call {
        args: Vec<u8>,
        export: String,
    },
    Schema,
}
impl Entry<'_> {
    fn prepare(self) -> Invocation {
        match self {
            Self::Call { args, export } => Invocation::Call {
                export: export.into(),
                args: args.to_vec(),
            },
            Self::Schema => Invocation::Schema,
        }
    }
}
fn error(error: wasmtime::Error) -> anyhow::Error {
    call::wasm_error(error)
}
fn host_error(error: anyhow::Error) -> wasmtime::Error {
    wasmtime::Error::from_anyhow(error)
}

fn copy_out(memory: &SharedMemory, pointer: u32, length: u32) -> Result<Vec<u8>> {
    anyhow::ensure!(
        length as usize <= loom_proto::TRACE_MAX_BLOB_BYTES,
        "guest message exceeds byte limit"
    );
    // Atomic word copies: another guest thread may write this memory, so a plain
    // memcpy would be a data race even when the guest claims exclusive ownership.
    crate::shared_copy::read(memory.data(), pointer as usize, length as usize)
}
fn copy_in(memory: &SharedMemory, pointer: u32, bytes: &[u8]) -> Result<()> {
    anyhow::ensure!(
        bytes.len() <= loom_proto::TRACE_MAX_BLOB_BYTES,
        "guest message exceeds byte limit"
    );
    crate::shared_copy::write(memory.data(), pointer as usize, bytes)
}
/// Wasm exports alignment zero when there is no TLS block. No allocation is
/// made in that case; use alignment one for bookkeeping and checked arithmetic.
fn tls_alignment(size: u32, alignment: u32) -> Result<u32> {
    if size == 0 {
        return Ok(1);
    }
    anyhow::ensure!(
        alignment.is_power_of_two() && alignment <= 65536,
        "invalid guest TLS alignment"
    );
    Ok(alignment)
}
async fn allocate(caller: &mut Caller<'_, Guest>, size: u32, align: u32) -> Result<u32> {
    let function = caller
        .get_export("loom_alloc")
        .and_then(|e| e.into_func())
        .context("missing loom_alloc")?
        .typed::<(i32, i32), i32>(&*caller)
        .map_err(error)?;
    let pointer = function
        .call_async(&mut *caller, (size as i32, align as i32))
        .await
        .map_err(error)? as u32;
    anyhow::ensure!(size == 0 || pointer != 0, "guest allocation failed");
    anyhow::ensure!(
        align.is_power_of_two() && pointer.is_multiple_of(align),
        "guest allocation has invalid alignment"
    );
    anyhow::ensure!(
        (pointer as u64 + size as u64) <= caller.data().execution.memory.data_size() as u64,
        "guest allocation outside memory"
    );
    Ok(pointer)
}
async fn respond(caller: &mut Caller<'_, Guest>, bytes: Vec<u8>) -> Result<i64> {
    let pointer = allocate(caller, bytes.len().try_into()?, 1).await?;
    copy_in(&caller.data().execution.memory, pointer, &bytes)?;
    Ok(((bytes.len() as u64) << 32 | pointer as u64) as i64)
}
/// Bytes a single kernel call may read from the guest, summed over its buffers.
const KERNEL_MAX_BYTES: usize = 256 * 1024 * 1024;
/// Write `bytes` then `tag` into one fresh guest allocation and return it packed
/// as `length << 32 | pointer` (the length includes the tag).
async fn respond_tagged(caller: &mut Caller<'_, Guest>, bytes: Vec<u8>, tag: u8) -> Result<i64> {
    // A reply the guest cannot hold is an error the guest can see, not a trap.
    let (bytes, tag) = if bytes.len() + 1 > KERNEL_MAX_BYTES {
        let message = format!(
            "kernel reply of {} bytes exceeds {KERNEL_MAX_BYTES}",
            bytes.len()
        );
        caller.data().execution.runtime.note_kernel_failure();
        (message.into_bytes(), 1)
    } else {
        (bytes, tag)
    };
    let total = bytes.len() + 1;
    let pointer = allocate(caller, total.try_into()?, 1).await?;
    let memory = caller.data().execution.memory.clone();
    crate::shared_copy::write(memory.data(), pointer as usize, &bytes)?;
    crate::shared_copy::write(memory.data(), pointer as usize + bytes.len(), &[tag])?;
    Ok(((total as u64) << 32 | pointer as u64) as i64)
}
pub(super) enum Entry<'a> {
    /// `args` is one DAG-CBOR array of typed arguments, never decoded here.
    Call {
        args: &'a [u8],
        export: &'a str,
    },
    Schema,
}

struct Buffer {
    pointer: i32,
    length: i32,
}
#[cfg(test)]
mod tests;
