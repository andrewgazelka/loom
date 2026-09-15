//! Fresh V8 isolates using Loom's caller-owned effect and transaction boundary.
mod codec;
mod control;
mod execution;
mod pool;

/// Immutable guest contract and engine version used by definition identities.
pub const ABI_VERSION: &str = "loom-v8/1/v8-152.2.0";

use anyhow::{Result, ensure};
use loom_sandbox::{CallEffects, GuestFailure, Sandbox};
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};

/// Per-engine admission and per-invocation execution limits. The heap limit is
/// a V8 managed-heap budget, not a bound on total process RSS. See the README.
#[derive(Clone, Debug)]
pub struct Limits {
    pub timeout: Duration,
    pub heap_bytes: usize,
    pub max_source_bytes: usize,
    pub max_message_bytes: usize,
    pub max_pending_effects: usize,
    pub workers: usize,
    pub queue_capacity: usize,
    pub max_reentrant_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            heap_bytes: 32 * 1024 * 1024,
            max_source_bytes: 1024 * 1024,
            max_message_bytes: 1024 * 1024,
            max_pending_effects: 64,
            workers: 2,
            queue_capacity: 32,
            max_reentrant_depth: 32,
        }
    }
}

impl Limits {
    fn validate(&self) -> Result<()> {
        ensure!(!self.timeout.is_zero(), "V8 timeout must be positive");
        ensure!(
            self.timeout <= Duration::from_secs(3600),
            "V8 timeout exceeds one hour"
        );
        ensure!(
            self.heap_bytes >= 8 * 1024 * 1024,
            "V8 heap must be at least 8 MiB"
        );
        ensure!(
            self.heap_bytes <= 1024 * 1024 * 1024,
            "V8 heap exceeds 1 GiB"
        );
        ensure!(
            self.max_source_bytes > 0 && self.max_source_bytes <= i32::MAX as usize,
            "invalid V8 source limit"
        );
        ensure!(
            self.max_message_bytes > 0 && self.max_message_bytes <= i32::MAX as usize,
            "invalid V8 message limit"
        );
        ensure!(
            self.max_pending_effects > 0 && self.max_pending_effects <= 65536,
            "invalid V8 pending effect limit"
        );
        ensure!(
            self.workers > 0 && self.workers <= 64,
            "V8 workers must be between 1 and 64"
        );
        ensure!(
            self.queue_capacity > 0 && self.queue_capacity <= 65536,
            "invalid V8 queue capacity"
        );
        ensure!(
            self.max_reentrant_depth > 0 && self.max_reentrant_depth <= 64,
            "V8 reentrant depth must be between 1 and 64"
        );
        Ok(())
    }
}

#[derive(Clone)]
pub struct V8Engine {
    pool: Arc<pool::Pool>,
}

#[derive(Clone)]
pub struct V8Sandbox {
    engine: V8Engine,
    program: Arc<Program>,
}

struct Program {
    source: String,
    code_cache: Vec<u8>,
    schema: String,
}

/// Counts completed compilations and V8-accepted code-cache consumptions.
#[derive(Clone, Copy, Debug, Default)]
pub struct CacheStats {
    pub compilations: u64,
    pub cache_hits: u64,
}

struct EffectRequest {
    descriptor: Value,
    reply: std::sync::mpsc::SyncSender<Value>,
}

impl V8Engine {
    pub fn new(limits: Limits) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            pool: Arc::new(pool::Pool::new(limits)?),
        })
    }

    pub fn cache_stats(&self) -> CacheStats {
        self.pool.cache_stats()
    }

    /// Validate main and optional LOOM_SCHEMA without granting host effects.
    /// Compilation executes top-level initialization under the same limits as
    /// a call; source and portable compiled bytes are retained by the sandbox.
    pub async fn compile(&self, source: &str) -> Result<V8Sandbox> {
        guest_ensure(
            source.len() <= self.pool.limits.max_source_bytes,
            "JavaScript source exceeds byte limit",
        )?;
        let control = control::Control::new(self.pool.limits.timeout);
        let _cancel = control::CancelOnDrop(control.clone());
        let (reply, receiver) = oneshot::channel();
        self.pool.submit(pool::Job {
            control: control.clone(),
            task: pool::Task::Compile {
                source: source.to_owned(),
                reply,
            },
        })?;
        let program = tokio::time::timeout(self.pool.limits.timeout, receiver)
            .await
            .map_err(|_| guest("JavaScript execution timed out"))?
            .map_err(|_| anyhow::anyhow!("V8 compilation worker stopped"))??;
        Ok(V8Sandbox {
            engine: self.clone(),
            program: Arc::new(program),
        })
    }
}

impl V8Sandbox {
    pub fn schema(&self) -> &str {
        &self.program.schema
    }
}

impl Sandbox for V8Sandbox {
    fn call<'a>(
        &'a self,
        args: Value,
        effects: &'a mut dyn CallEffects,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            guest_ensure(
                args.is_array(),
                "JavaScript call arguments must be an array",
            )?;
            let args = encode_message(&args, self.engine.pool.limits.max_message_bytes)?;
            let control = control::Control::new(self.engine.pool.limits.timeout);
            let _cancel = control::CancelOnDrop(control.clone());
            let (reply, mut receiver) = oneshot::channel();
            let (sender, mut requests) = mpsc::channel(self.engine.pool.limits.max_pending_effects);
            self.engine.pool.submit(pool::Job {
                control,
                task: pool::Task::Call {
                    program: self.program.clone(),
                    args,
                    effects: sender,
                    reply,
                },
            })?;
            let execution = async {
                loop {
                    tokio::select! {
                        biased;
                        // Effects already issued by the guest belong to this
                        // transaction even when its returned promise is settled.
                        Some(request) = requests.recv() => {
                            let output = effects.perform(request.descriptor).await?;
                            encode_message(&output, self.engine.pool.limits.max_message_bytes)?;
                            let _ = request.reply.send(output);
                        }
                        result = &mut receiver => {
                            return result.map_err(|_| anyhow::anyhow!("V8 execution worker stopped"))?;
                        }
                    }
                }
            };
            tokio::time::timeout(self.engine.pool.limits.timeout, execution)
                .await
                .map_err(|_| guest("JavaScript execution timed out"))?
        })
    }
}

fn guest(message: impl Into<String>) -> anyhow::Error {
    GuestFailure::new(message).into()
}

fn guest_ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(guest(message))
    }
}

fn encode_message(value: &Value, limit: usize) -> Result<String> {
    validate_host_value(value, 0)?;
    // serde_json's writer stops at the byte budget rather than allocating an
    // arbitrarily large intermediate String for an already-owned host value.
    struct LimitedWriter {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for LimitedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("message exceeds byte limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = LimitedWriter {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| guest(format!("Loom message encoding failed: {error}")))?;
    String::from_utf8(writer.bytes).map_err(Into::into)
}

fn validate_host_value(value: &Value, depth: usize) -> Result<()> {
    guest_ensure(depth <= 128, "Loom JSON nesting exceeds 128 levels")?;
    match value {
        Value::Number(number) => {
            // JSON.parse produces binary64 Numbers. Refuse exact Rust integers
            // it would silently round; decimal strings preserve larger values.
            if let Some(integer) = number.as_i64() {
                guest_ensure(
                    (-9_007_199_254_740_991..=9_007_199_254_740_991).contains(&integer),
                    "Loom integer exceeds JavaScript's safe integer range",
                )?;
            } else if let Some(integer) = number.as_u64() {
                guest_ensure(
                    integer <= 9_007_199_254_740_991,
                    "Loom integer exceeds JavaScript's safe integer range",
                )?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_host_value(value, depth + 1)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_host_value(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}
