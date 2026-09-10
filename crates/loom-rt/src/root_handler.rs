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
                Box::pin(async move { bail!("unsupported ability: {}", operation(&request.desc)?) })
            }
        }
    }
}
fn operation(desc: &Value) -> Result<&str> {
    desc.get("op")
        .and_then(Value::as_str)
        .context("descriptor op required")
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
            let op = operation(&desc)?;
            let scheduler = matches!(op, "fork" | "join" | "all" | "race" | "call");
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
            let tracked = !scheduler || op == "race" || !effects.permits(op);
            let guard = if tracked {
                match execution.begin(scope, occurrence, &desc, op == "race")? {
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
            if effects.actor_id.is_some() && tracked {
                execution.checkpoint(&runtime.inner.store)?;
            }
            recorded?;
            outcome
        })
    }
}

struct Scheduling;
impl RootHandler for Scheduling {
    fn handle<'a>(&'a self, request: Request<'a>, next: Next<'a>) -> HandlerFuture<'a> {
        Box::pin(async move {
            let runtime = request.runtime;
            let scope = request.scope;
            let occurrence = request.occurrence;
            let effects = &request.effects;
            let op = operation(&request.desc)?;
            let args = request.desc.get("args").cloned().unwrap_or(Value::Null);
            let hash = if matches!(op, "send" | "spawn") {
                blake3::hash(&encode(&request.desc)?).to_hex().to_string()
            } else {
                String::new()
            };
            match op {
                "all" | "race" => {
                    let mut args = args;
                    let descs = match args.get_mut("descs").map(Value::take) {
                        Some(Value::Array(descs)) => descs,
                        _ => bail!("descs required"),
                    };
                    let child_scope = format!("{scope}/{op}:{occurrence}");
                    let futures = descs
                        .into_iter()
                        .enumerate()
                        .map(|(index, desc)| match &effects.root_dispatch {
                            Some(dispatch) => {
                                dispatch.dispatch(desc, &child_scope, index as i64, effects.clone())
                            }
                            None => runtime.dispatch_root(
                                desc,
                                &child_scope,
                                index as i64,
                                effects.clone(),
                            ),
                        })
                        .collect::<Vec<_>>();
                    if op == "all" {
                        let results = futures::future::try_join_all(futures).await?;
                        return Ok(EffectOutput {
                            bytes: loom_proto::encode_host_array(
                                results.iter().map(|result| result.bytes.as_slice()),
                            )
                            .map_err(anyhow::Error::msg)?,
                        });
                    }
                    if futures.is_empty() {
                        bail!("race requires at least one descriptor");
                    }
                    return futures::future::select_all(futures).await.0;
                }
                "call" => {
                    return runtime
                        .call_scoped(
                            required_str(&args, "def")?,
                            args.get("args").cloned().unwrap_or(Value::Null),
                            &format!("{scope}/call:{occurrence}"),
                            effects.clone(),
                        )
                        .await;
                }
                "fork" => {
                    let hash = required_str(&args, "def")?.to_owned();
                    let args = args.get("args").cloned().unwrap_or(Value::Null);
                    let child_runtime = runtime.clone();
                    let child_scope = format!("{scope}/fork:{occurrence}");
                    let id = child_scope.clone();
                    let child_effects = effects.clone();
                    let task = tokio::spawn(async move {
                        child_runtime
                            .call_scoped(&hash, args, &child_scope, child_effects)
                            .await
                    });
                    runtime.inner.fibers.lock().unwrap().insert(
                        id.clone(),
                        FiberTask {
                            scope: scope.into(),
                            task,
                        },
                    );
                    return EffectOutput::value(&json!(id));
                }
                "join" => {
                    let mut out = Vec::new();
                    for id in args
                        .get("fibers")
                        .and_then(Value::as_array)
                        .context("fibers required")?
                    {
                        let mut task = runtime
                            .take_fiber(id.as_str().context("fiber id must be string")?, scope)?;
                        out.push((&mut task.task).await??);
                    }
                    return Ok(EffectOutput {
                        bytes: loom_proto::encode_host_array(
                            out.iter()
                                .map(|result: &EffectOutput| result.bytes.as_slice()),
                        )
                        .map_err(anyhow::Error::msg)?,
                    });
                }
                "send" => {
                    let key = format!("{scope}:{occurrence}:{hash}");
                    let message = runtime.inner.store.enqueue_once(
                        required_str(&args, "actor")?,
                        &args.get("msg").cloned().unwrap_or(Value::Null),
                        &key,
                    )?;
                    return runtime
                        .schedule_message(message)
                        .and_then(|result| EffectOutput::value(&result));
                }
                "spawn" => {
                    let key = format!("spawn:{scope}:{occurrence}:{hash}");
                    let actor_id = blake3::hash(key.as_bytes()).to_hex().to_string();
                    let actor = runtime
                        .spawn_identified(
                            required_str(&args, "def")?,
                            args.get("state").cloned().unwrap_or(Value::Null),
                            actor_id,
                        )
                        .await?;
                    let result = serde_json::to_value(actor)?;
                    return EffectOutput::value(&result);
                }
                _ => return next.run(request).await,
            }
        })
    }
}

struct Memo;
impl RootHandler for Memo {
    fn handle<'a>(&'a self, request: Request<'a>, next: Next<'a>) -> HandlerFuture<'a> {
        Box::pin(async move {
            let runtime = request.runtime;
            let op = operation(&request.desc)?;
            let args = request.desc.get("args").cloned().unwrap_or(Value::Null);
            let hash = if matches!(op, "cas.get" | "cas.put" | "exec") {
                blake3::hash(&encode(&request.desc)?).to_hex().to_string()
            } else {
                String::new()
            };
            let class = match op {
                "cas.get" | "cas.put" => "hermetic",
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
            if memoized {
                if let Some(result) = runtime.inner.store.effect_get(&hash, "global", 0)? {
                    return EffectOutput::value(&result);
                }
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
            let op = operation(&request.desc)?;
            let args = request.desc.get("args").cloned().unwrap_or(Value::Null);
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
                "cas.put" => {
                    let hash = runtime.inner.store.put_value("blob", &args)?;
                    runtime
                        .inner
                        .store
                        .reference(&hash, loom_proto::DAG_CBOR_CODEC)?
                }
                "cas.get" => runtime
                    .inner
                    .store
                    .get_value(required_str(&args, "hash")?)?
                    .context("CAS value not found")?,
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
mod tests {
    use super::*;

    struct Answer {
        calls: AtomicU64,
    }
    impl RootHandler for Answer {
        fn handle<'a>(&'a self, _request: Request<'a>, _next: Next<'a>) -> HandlerFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::Relaxed);
                EffectOutput::value(&json!(41))
            })
        }
    }
    struct Increment;
    impl RootHandler for Increment {
        fn handle<'a>(&'a self, request: Request<'a>, next: Next<'a>) -> HandlerFuture<'a> {
            Box::pin(async move {
                let output = next.run(request).await?.decode()?;
                EffectOutput::value(&json!(output.as_u64().context("integer required")? + 1))
            })
        }
    }

    #[tokio::test]
    async fn recording_wraps_forwarding_and_replay_skips_downstream_handlers() -> Result<()> {
        let runtime = Runtime::new(Store::memory()?)?;
        let answer = Answer {
            calls: AtomicU64::new(0),
        };
        let increment = Increment;
        let handlers: [&dyn RootHandler; 3] = [&RECORDING, &increment, &answer];
        let execution = trace::ExecutionTrace::fresh("composed");
        let output = Next {
            handlers: &handlers,
        }
        .run(Request {
            runtime: &runtime,
            desc: json!({"op":"test.answer"}),
            scope: "composed",
            occurrence: 0,
            effects: EffectContext {
                trace: Some(execution.clone()),
                ..Default::default()
            },
        })
        .await?;
        assert_eq!(output.decode()?, json!(42));
        let result = Ok(output);
        let replay = trace::ExecutionTrace::loaded(execution.snapshot(Some(&result), true)?)?;
        let output = Next {
            handlers: &handlers,
        }
        .run(Request {
            runtime: &runtime,
            desc: json!({"op":"test.answer"}),
            scope: "composed",
            occurrence: 0,
            effects: EffectContext {
                trace: Some(replay),
                ..Default::default()
            },
        })
        .await?;
        assert_eq!(output.decode()?, json!(42));
        assert_eq!(answer.calls.load(Ordering::Relaxed), 1);
        Ok(())
    }

    struct GuestDispatch {
        calls: AtomicU64,
    }
    impl RootDispatch for GuestDispatch {
        fn dispatch<'a>(
            &'a self,
            _desc: Value,
            scope: &'a str,
            occurrence: i64,
            _effects: EffectContext,
        ) -> HandlerFuture<'a> {
            Box::pin(async move {
                assert_eq!(scope, "root/all:0");
                assert_eq!(occurrence, 0);
                self.calls.fetch_add(1, Ordering::Relaxed);
                EffectOutput::value(&json!(42))
            })
        }
    }

    #[tokio::test]
    async fn scheduler_children_reenter_guest_context_but_definition_delegation_drops_it()
    -> Result<()> {
        let runtime = Runtime::new(Store::memory()?)?;
        let dispatch = Arc::new(GuestDispatch {
            calls: AtomicU64::new(0),
        });
        let effects = EffectContext {
            root_dispatch: Some(dispatch.clone()),
            ..Default::default()
        };
        assert!(
            effects
                .delegated("another-definition", None)
                .root_dispatch
                .is_none()
        );
        let output = runtime
            .dispatch_root(
                json!({"op":"all","args":{"descs":[{"op":"custom"}]}}),
                "root",
                0,
                effects,
            )
            .await?;
        assert_eq!(output.decode()?, json!([42]));
        assert_eq!(dispatch.calls.load(Ordering::Relaxed), 1);
        Ok(())
    }

    #[test]
    fn declared_rows_intersect_capabilities_instead_of_expanding_them() {
        let effects = EffectContext::default()
            .delegated("def", Some(&["sleep".into(), "fs.read".into()]))
            .with_declared(Some(&["sleep".into(), "exec".into()]));
        assert!(effects.permits("sleep"));
        assert!(!effects.permits("fs.read"));
        assert!(!effects.permits("exec"));
    }
}
