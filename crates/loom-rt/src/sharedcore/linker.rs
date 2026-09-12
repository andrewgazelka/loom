use super::*;

pub(super) fn linker(
    execution: &Arc<Execution>,
    store: &wasmtime::Store<Guest>,
) -> Result<wasmtime::Linker<Guest>> {
    let mut linker = wasmtime::Linker::new(&execution.runtime.inner.core_engine);
    linker
        .define(store, "env", "memory", execution.memory.clone())
        .map_err(error)?;
    linker
        .func_wrap_async(
            "loom",
            "perform",
            |mut caller: Caller<'_, Guest>, (pointer, length): (i32, i32)| {
                Box::new(async move {
                    let result: Result<i64> = async {
                        let started = Instant::now();
                        caller.data_mut().last_effect_error = None;
                        let execution = caller.data().execution.clone();
                        let bytes = copy_out(&execution.memory, pointer as u32, length as u32)?;
                        let mut descriptor =
                            loom_proto::decode(&bytes).map_err(anyhow::Error::msg)?;
                        if let Some(definition) = &execution.effects.def_hash {
                            resolve_self(&mut descriptor, definition);
                        }
                        let occurrence = caller.data().occurrence;
                        caller.data_mut().occurrence += 1;
                        if let Some(bytes) =
                            handlers::dispatch(&mut caller, &descriptor, occurrence).await?
                        {
                            let response = respond(&mut caller, bytes).await?;
                            let mut samples = execution.handler_round_trip_us.lock().unwrap();
                            if samples.len() == 100_000 {
                                samples.drain(..50_000);
                            }
                            samples.push(started.elapsed().as_secs_f64() * 1_000_000.0);
                            return Ok(response);
                        }
                        anyhow::ensure!(
                            !execution.pure,
                            "effects forbidden in pure core execution"
                        );
                        let scope = caller.data().scope.clone();
                        let effects = execution.effects.clone();
                        caller.data_mut().permit.take();
                        let output = execution
                            .runtime
                            .dispatch_root(descriptor, &scope, occurrence, effects)
                            .await;
                        caller.data_mut().permit.take();
                        caller.data_mut().permit = Some(execution.permit().await?);
                        let bytes = match output {
                            Ok(output) => {
                                // The effect owner already produced canonical bytes.
                                let mut envelope = Vec::with_capacity(4 + output.bytes.len());
                                envelope.extend_from_slice(&[0xa1, 0x62, b'o', b'k']);
                                envelope.extend_from_slice(&output.bytes);
                                envelope
                            }
                            Err(error) => encode(&json!({"error":format!("{error:#}")}))?,
                        };
                        execution
                            .runtime
                            .inner
                            .effect_wire_bytes
                            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        respond(&mut caller, bytes).await
                    }
                    .await;
                    result.map_err(host_error)
                })
            },
        )
        .map_err(error)?;
    linker
        .func_wrap_async(
            "loom",
            "spawn",
            |mut caller: Caller<'_, Guest>, (function, data, detached): (i32, i32, i32)| {
                Box::new(async move {
                    spawn(&mut caller, function, data, detached)
                        .await
                        .map_err(host_error)
                })
            },
        )
        .map_err(error)?;
    linker.func_wrap_async("loom", "join", |mut caller: Caller<'_, Guest>, (id,): (i64,)| Box::new(async move {
        let result: Result<i32> = async {
            let execution = caller.data().execution.clone();
            let job = execution.jobs.lock().unwrap().get(&(id as u64)).cloned().context("unknown shared job")?;
            anyhow::ensure!(job.detached || job.parent == caller.data().scope, "shared job belongs to another scope");
            caller.data_mut().permit.take();
            let status = loop {
                let notification = job.done.notified();
                let cancelled = execution.cancellation.notified();
                execution.check()?;
                let result = job.result.lock().unwrap().clone();
                if let Some(result) = result {
                    match result {
                        Ok(()) => break 0,
                        Err(_) if job.detached => break 1,
                        Err(failure) => return Err(failure.into_error()),
                    }
                }
                tokio::select! { _ = notification => {}, _ = cancelled => bail!("shared execution cancelled"), _ = tokio::time::sleep_until(execution.deadline.into()) => bail!("shared execution deadline exceeded") }
            };
            caller.data_mut().permit = Some(execution.permit().await?);
            Ok(status)
        }.await;
        result.map_err(host_error)
    })).map_err(error)?;
    linker
        .func_wrap_async(
            "loom",
            "join_error",
            |mut caller: Caller<'_, Guest>, (id,): (i64,)| {
                Box::new(async move {
                    let result: Result<i64> = async {
                        let job = caller
                            .data()
                            .execution
                            .jobs
                            .lock()
                            .unwrap()
                            .get(&(id as u64))
                            .cloned()
                            .context("unknown shared job")?;
                        anyhow::ensure!(job.detached, "join_error requires a detached job");
                        let message = match job.result.lock().unwrap().as_ref() {
                            Some(Err(failure)) => failure.message.clone(),
                            _ => bail!("shared job has no error"),
                        };
                        respond(&mut caller, message.into_bytes()).await
                    }
                    .await;
                    result.map_err(host_error)
                })
            },
        )
        .map_err(error)?;
    handlers::link(&mut linker)?;
    Ok(linker)
}
async fn spawn(
    caller: &mut Caller<'_, Guest>,
    function: i32,
    data: i32,
    detached: i32,
) -> Result<i64> {
    anyhow::ensure!(
        matches!(detached, 0 | 1),
        "invalid shared job detached flag"
    );
    let detached = detached == 1;
    let execution = caller.data().execution.clone();
    execution.check()?;
    if execution
        .job_count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < MAX_JOBS as u64).then_some(count + 1)
        })
        .is_err()
    {
        return Ok(0);
    }
    let occurrence = caller.data().occurrence;
    caller.data_mut().occurrence += 1;
    let parent = caller.data().scope.clone();
    let scope = format!("{parent}/spawn:{occurrence}");
    let digest = blake3::hash(scope.as_bytes());
    let id = u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap()) & i64::MAX as u64;
    {
        let jobs = execution.jobs.lock().unwrap();
        if jobs.len() >= MAX_JOBS {
            return Ok(0);
        }
        anyhow::ensure!(
            id != 0 && !jobs.contains_key(&id),
            "shared job identity collision"
        );
    }
    let stack = allocate(caller, STACK_BYTES, 16).await?;
    let tls_size = caller
        .get_export("__tls_size")
        .and_then(|e| e.into_global())
        .context("missing TLS size")?
        .get(&mut *caller)
        .i32()
        .context("invalid TLS size")? as u32;
    let tls_align = caller
        .get_export("__tls_align")
        .and_then(|e| e.into_global())
        .context("missing TLS alignment")?
        .get(&mut *caller)
        .i32()
        .context("invalid TLS alignment")? as u32;
    let tls_align = tls_alignment(tls_size, tls_align)?;
    let tls = if tls_size == 0 {
        0
    } else {
        allocate(caller, tls_size, tls_align).await?
    };
    // Handler frames can borrow the spawning stack. handle_pop drains every
    // inheriting job, including detached jobs, before freeing the frame data.
    let handlers = caller.data().handlers.clone();
    let job = Arc::new(Job {
        handlers: handlers.iter().map(|frame| frame.id).collect(),
        parent,
        detached,
        result: Mutex::new(None),
        done: Notify::new(),
    });
    execution.jobs.lock().unwrap().insert(id, job.clone());
    let child = execution.clone();
    let child_job = job.clone();
    execution.schedule(async move {
        let result: Result<()> = async {
            let mut running = child
                .instantiate(
                    scope.clone(),
                    Some(Allocation {
                        stack: stack + STACK_BYTES,
                        tls,
                    }),
                    handlers,
                )
                .await?;
            let task = running
                .instance
                .get_typed_func::<(i32, i32), ()>(&mut running.store, "loom_task_run")
                .map_err(error)?;
            task.call_async(&mut running.store, (function, data))
                .await
                .map_err(|cause| running.error_context(cause))?;
            Ok(())
        }
        .await;
        let result = result.map_err(|error| {
            child.original_failure().unwrap_or_else(|| {
                ExecutionFailure::new(error.context(format!("shared job {scope}")))
            })
        });
        let cancel = result.is_err() && !child_job.detached;
        // Publish the cause before waking siblings through cancellation.
        *child_job.result.lock().unwrap() = Some(result);
        if cancel {
            child.cancel();
        }
        child_job.done.notify_waiters();
    })?;
    Ok(id as i64)
}
