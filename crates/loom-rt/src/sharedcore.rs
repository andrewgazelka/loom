//! Shared linear memory belongs to one execution. Stores never borrow host slices
//! of that memory, and aborted guest stacks remain allocated until every task ends.
use super::*;
use futures::{
    FutureExt,
    future::{AbortHandle, Abortable, RemoteHandle},
    task::SpawnExt,
};
use std::sync::atomic::{AtomicBool, AtomicU8};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use wasmtime::{
    Caller, Config, ExternType, Instance as CoreInstance, Module, SharedMemory, UpdateDeadline,
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

pub(super) fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config
        .wasm_threads(true)
        .shared_memory(true)
        .epoch_interruption(true);
    Engine::new(&config).map_err(|e| anyhow::anyhow!("{e:#}"))
}
struct Job {
    handlers: Vec<u64>,
    parent: String,
    detached: bool,
    result: Mutex<Option<std::result::Result<(), String>>>,
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
    handler_failure: Mutex<Option<String>>,
    runtime: Runtime,
    module: Module,
    memory: SharedMemory,
    effects: EffectContext,
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
enum Invocation {
    Call { args: Vec<u8> },
    Run { state: Vec<u8>, message: Vec<u8> },
    Fold { state: Vec<u8>, event: Vec<u8> },
    Validate,
}
impl Entry<'_> {
    fn prepare(self) -> Result<Invocation> {
        Ok(match self {
            Self::Call { args } => Invocation::Call {
                args: encode(args)?,
            },
            Self::Run { state, message } => Invocation::Run {
                state: encode(state)?,
                message: encode(message)?,
            },
            Self::Fold { state, event } => Invocation::Fold {
                state: encode(state)?,
                event: encode(event)?,
            },
            Self::Validate => Invocation::Validate,
        })
    }
}
fn error(error: wasmtime::Error) -> anyhow::Error {
    anyhow::anyhow!("{error:#}")
}
fn host_error(error: anyhow::Error) -> wasmtime::Error {
    wasmtime::Error::msg(format!("{error:#}"))
}

fn copy_out(memory: &SharedMemory, pointer: u32, length: u32) -> Result<Vec<u8>> {
    anyhow::ensure!(
        length as usize <= loom_proto::TRACE_MAX_BLOB_BYTES,
        "guest message exceeds byte limit"
    );
    let start = pointer as usize;
    let end = start
        .checked_add(length as usize)
        .context("guest range overflow")?;
    let data = memory.data();
    anyhow::ensure!(end <= data.len(), "guest message outside shared memory");
    Ok(data[start..end]
        .iter()
        .map(|cell| {
            // Atomic accesses are required even when the guest claims exclusive ownership.
            unsafe { AtomicU8::from_ptr(cell.get()).load(Ordering::Relaxed) }
        })
        .collect())
}
fn copy_in(memory: &SharedMemory, pointer: u32, bytes: &[u8]) -> Result<()> {
    let start = pointer as usize;
    let end = start
        .checked_add(bytes.len())
        .context("guest range overflow")?;
    anyhow::ensure!(
        bytes.len() <= loom_proto::TRACE_MAX_BLOB_BYTES,
        "guest message exceeds byte limit"
    );
    let data = memory.data();
    anyhow::ensure!(end <= data.len(), "guest allocation outside shared memory");
    for (cell, byte) in data[start..end].iter().zip(bytes) {
        unsafe {
            AtomicU8::from_ptr(cell.get()).store(*byte, Ordering::Relaxed);
        }
    }
    Ok(())
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
pub(super) enum Entry<'a> {
    Call {
        args: &'a Value,
    },
    Run {
        state: &'a Value,
        message: &'a Value,
    },
    Fold {
        state: &'a Value,
        event: &'a Value,
    },
    Validate,
}

struct Buffer {
    pointer: i32,
    length: i32,
}
#[cfg(test)]
mod tests;
