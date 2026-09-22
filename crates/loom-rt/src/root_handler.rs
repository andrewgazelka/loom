//! Outermost effect handlers. Only operations forwarded out of guest handler
//! scopes enter this chain. Recording wraps the same forwarding protocol as
//! scheduling and memoization; replay short-circuits that continuation.
use super::*;
use std::{future::Future, pin::Pin};

type HandlerFuture<'a> = Pin<Box<dyn Future<Output = Result<EffectOutput>> + Send + 'a>>;

struct Request<'a> {
    runtime: &'a Runtime,
    desc: Value,
    scope: &'a str,
    occurrence: i64,
    effects: EffectContext,
}

trait RootHandler: Sync {
    fn handle<'a>(&'a self, request: Request<'a>, next: Next<'a>) -> HandlerFuture<'a>;
}

/// A one-shot forwarding continuation. A handler can reply, fail, or forward
/// and transform the result; dropping it cancels all work below that handler.
struct Next<'a> {
    handlers: &'a [&'a dyn RootHandler],
}
impl<'a> Next<'a> {
    fn run(self, request: Request<'a>) -> HandlerFuture<'a> {
        match self.handlers.split_first() {
            Some((handler, rest)) => handler.handle(request, Next { handlers: rest }),
            None => {
                Box::pin(
                    async move { bail!("unsupported effect: {}", effect_name(&request.desc)?) },
                )
            }
        }
    }
}
fn effect_name(desc: &Value) -> Result<&str> {
    desc.get("op")
        .and_then(Value::as_str)
        .context("effect op required")
}

static RECORDING: Recording = Recording;
static SCHEDULING: Scheduling = Scheduling;
static MEMO: Memo = Memo;
static BUILTINS: Builtins = Builtins;
static HANDLERS: [&'static dyn RootHandler; 4] = [&RECORDING, &SCHEDULING, &MEMO, &BUILTINS];

pub(super) fn dispatch<'a>(
    runtime: &'a Runtime,
    desc: Value,
    scope: &'a str,
    occurrence: i64,
    effects: EffectContext,
) -> HandlerFuture<'a> {
    // Check before replay and memo lookup, which can otherwise return data from
    // a prior host-authorized execution without reaching the native handler.
    if effects.root.is_none()
        && let Ok(op) = effect_name(&desc)
        && let Err(error) = runtime.require_host_effect(op)
    {
        return Box::pin(async move { Err(error) });
    }
    if let Some(sender) = effects.root.clone() {
        return Box::pin(async move {
            let descriptor = match effect_name(&desc) {
                Ok(op) if effects.permits(op) => Ok(desc),
                Ok(op) => Err(GuestFailure::new(format!("effect {op} is not allowed"))),
                Err(error) => Err(GuestFailure::new(error.to_string())),
            };
            call::dispatch(&sender, descriptor).await
        });
    }
    Next {
        handlers: &HANDLERS,
    }
    .run(Request {
        runtime,
        desc,
        scope,
        occurrence,
        effects,
    })
}

struct Recording;
impl RootHandler for Recording {
    fn handle<'a>(&'a self, request: Request<'a>, next: Next<'a>) -> HandlerFuture<'a> {
        Box::pin(async move {
            let Request {
                runtime,
                desc,
                scope,
                occurrence,
                effects,
            } = request;
            let op = effect_name(&desc)?;
            let scheduler = op == "call";
            let execution = effects
                .trace
                .clone()
                .unwrap_or_else(|| trace::ExecutionTrace::fresh(scope));
            let mut effects = effects;
            effects.trace = Some(execution.clone());
            if !scheduler
                && effects.permits(op)
                && let Some(definition_hash) = &effects.def_hash
            {
                execution.observe(definition_hash, op)?;
            }
            let tracked = !scheduler || !effects.permits(op);
            let guard = if tracked {
                match execution.begin(scope, occurrence, &desc)? {
                    trace::StartedEffect::Replayed(output) => {
                        anyhow::ensure!(effects.permits(op), "effect {op} is not allowed");
                        return Ok(output);
                    }
                    trace::StartedEffect::Recorded(guard) => Some(guard),
                }
            } else {
                None
            };
            let outcome = if !effects.permits(op) {
                Err(anyhow::anyhow!(
                    "effect {op} is not allowed for definition {}",
                    effects.def_hash.as_deref().unwrap_or("<host>")
                ))
            } else {
                next.run(Request {
                    runtime,
                    desc,
                    scope,
                    occurrence,
                    effects: effects.clone(),
                })
                .await
            };
            let recorded = guard.map(|guard| guard.finish(&outcome)).transpose();
            recorded?;
            outcome
        })
    }
}

/// The `Value`-shaped `{"op":"call","args":{"def","entry","args":[...]}}`
/// descriptor is the JavaScript caller's form (V8 has no DAG-CBOR). This
/// adapter encodes its argument array once and hands the same request the
/// `loom.call` import builds to `Runtime::isolated_call`; core guests never
/// reach it (the `perform` import refuses the op).
struct Scheduling;
impl RootHandler for Scheduling {
    fn handle<'a>(&'a self, request: Request<'a>, next: Next<'a>) -> HandlerFuture<'a> {
        Box::pin(async move {
            let runtime = request.runtime;
            let scope = request.scope;
            let occurrence = request.occurrence;
            let effects = &request.effects;
            let op = effect_name(&request.desc)?;
            let args = request.desc.get("args").cloned().unwrap_or(Value::Null);
            match op {
                "call" => {
                    let target =
                        loom_proto::isolated::Target::from_hex(required_str(&args, "def")?)?;
                    let entry = match args.get("entry") {
                        None | Some(Value::Null) => "",
                        Some(entry) => entry.as_str().context("call entry must be a string")?,
                    };
                    anyhow::ensure!(
                        entry.len() <= loom_proto::isolated::MAX_ENTRY_BYTES,
                        "call entry name exceeds {} bytes",
                        loom_proto::isolated::MAX_ENTRY_BYTES
                    );
                    let positional = args.get("args").cloned().unwrap_or(json!([]));
                    let (argc, payload) = positional_payload(&positional)?;
                    let bytes = runtime
                        .isolated_call(
                            loom_proto::isolated::Request {
                                target,
                                entry,
                                argc,
                                payload: &payload,
                            },
                            scope,
                            occurrence,
                            effects,
                        )
                        .await?;
                    Ok(EffectOutput { bytes })
                }
                _ => next.run(request).await,
            }
        })
    }
}

struct Memo;
impl RootHandler for Memo {
    fn handle<'a>(&'a self, request: Request<'a>, next: Next<'a>) -> HandlerFuture<'a> {
        Box::pin(async move {
            let runtime = request.runtime;
            let op = effect_name(&request.desc)?;
            let args = request.desc.get("args").cloned().unwrap_or(Value::Null);
            let hash = if matches!(
                op,
                "cas.get" | "cas.put" | "cas.get_bytes" | "cas.put_bytes" | "exec"
            ) {
                blake3::hash(&encode(&request.desc)?).to_hex().to_string()
            } else {
                String::new()
            };
            let class = match op {
                "cas.get" | "cas.put" | "cas.get_bytes" | "cas.put_bytes" => "hermetic",
                "exec" if args.get("tree").and_then(Value::as_str).is_some() => "hermetic",
                "exec" if args.get("key").and_then(Value::as_str).is_some() => "keyed",
                _ => "observational",
            };
            let memoized = class == "hermetic" || class == "keyed";
            let lock = memoized.then(|| runtime.effect_lock(hash.clone()));
            let _guard = match &lock {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            if memoized && let Some(result) = runtime.inner.store.effect_get(&hash, "global", 0)? {
                return EffectOutput::value(&result);
            }

            let output = next.run(request).await?;
            if memoized {
                runtime
                    .inner
                    .store
                    .enqueue_effect(&hash, "global", 0, &output.decode()?)?;
            }
            Ok(output)
        })
    }
}

struct Builtins;
impl RootHandler for Builtins {
    fn handle<'a>(&'a self, request: Request<'a>, next: Next<'a>) -> HandlerFuture<'a> {
        Box::pin(async move {
            let runtime = request.runtime;
            let op = effect_name(&request.desc)?;
            let args = request.desc.get("args").cloned().unwrap_or(Value::Null);
            runtime.require_host_effect(op)?;
            let result = match op {
                "llm" => serde_json::to_value(
                    runtime
                        .inner
                        .model
                        .complete(serde_json::from_value(args.clone())?)
                        .await?,
                )?,
                "sleep" => {
                    let ms = args
                        .get("ms")
                        .and_then(Value::as_u64)
                        .context("ms required")?;
                    if ms > 86_400_000 {
                        bail!("sleep exceeds one day limit");
                    }
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Value::Null
                }
                "now" => json!(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_millis()
                ),
                "random" => json!(
                    (uuid::Uuid::new_v4().as_u128() as u64 & ((1u64 << 53) - 1)) as f64
                        / ((1u64 << 53) as f64)
                ),
                "cas.put" | "cas.get" | "cas.put_bytes" | "cas.get_bytes" => {
                    runtime.inner.store.guest_cas_effect(op, args)?
                }
                "exec" if args.get("tree").is_some() => runtime.hermetic_exec(&args).await?,
                "exec" => {
                    let program = required_str(&args, "program")?;
                    let mut command = tokio::process::Command::new(program);
                    if let Some(arguments) = args.get("args").and_then(Value::as_array) {
                        for argument in arguments {
                            command.arg(
                                argument
                                    .as_str()
                                    .context("exec arguments must be strings")?,
                            );
                        }
                    }
                    runtime
                        .execute_command(command, capture_paths(&args)?)
                        .await?
                }
                "fs.snapshot" => runtime.snapshot_tree(&args).await?,
                "fs.read" => runtime.read_machine_file(&args).await?,
                "fs.read_optional" => runtime.read_optional_machine_file(&args).await?,
                "fs.write" => runtime.write_machine_file(&args).await?,
                "fs.stat" => runtime.stat_machine_path(&args).await?,
                "fs.walk" => return runtime.walk_machine_directory(&args).await,
                "fs.list" => return runtime.list_machine_directory(&args).await,
                _ => return next.run(request).await,
            };

            EffectOutput::value(&result)
        })
    }
}

#[cfg(test)]
mod tests;
