use super::*;

impl Runtime {
    pub(crate) async fn core_call(
        &self,
        hash: &str,
        args: &Value,
        scope: &str,
        effects: &EffectContext,
    ) -> Result<EncodedCall> {
        self.core_execute(hash, scope, effects, false, Entry::Call { args })
            .await
    }
    pub(crate) async fn core_execute(
        &self,
        hash: &str,
        scope: &str,
        effects: &EffectContext,
        pure: bool,
        entry: Entry<'_>,
    ) -> Result<EncodedCall> {
        let start = Instant::now();
        let mut definition = self
            .inner
            .store
            .executable_definition(hash)?
            .context("definition not found")?;
        if definition.component_hash.is_none() {
            self.inner
                .resolver
                .as_ref()
                .context("definition has no built artifact")?
                .ensure_built(hash)
                .await?;
            definition = self
                .inner
                .store
                .executable_definition(hash)?
                .context("built definition disappeared")?;
        }
        let artifact = definition
            .component_hash
            .context("definition has no built artifact")?;
        let compile_lock = self
            .inner
            .component_locks
            .lock()
            .unwrap()
            .entry(artifact.clone())
            .or_default()
            .clone();
        let compile_guard = compile_lock.lock().await;
        let cached = self
            .inner
            .core_modules
            .lock()
            .unwrap()
            .get(&artifact)
            .cloned();
        let module = if let Some(module) = cached {
            module
        } else {
            let bytes = self
                .inner
                .store
                .get(&artifact)?
                .context("artifact missing")?;
            anyhow::ensure!(
                loom_proto::core_protocol::is_current(&bytes),
                "artifact {artifact} for definition {hash} is not an admitted core wasm module; rebuild the definition"
            );
            let engine = self.inner.core_engine.clone();
            let module =
                tokio::task::spawn_blocking(move || Module::new(&engine, &bytes).map_err(error))
                    .await??;
            self.inner
                .core_modules
                .lock()
                .unwrap()
                .insert(artifact.clone(), module.clone());
            module
        };
        drop(compile_guard);
        let mut memory_type = None;
        for import in module.imports() {
            if let ExternType::Memory(ty) = import.ty() {
                anyhow::ensure!(
                    import.module() == "env" && import.name() == "memory" && memory_type.is_none(),
                    "core must import exactly env.memory"
                );
                anyhow::ensure!(
                    ty.is_shared()
                        && !ty.is_64()
                        && ty
                            .maximum()
                            .is_some_and(|pages| pages * 65536 <= MAX_MEMORY),
                    "core memory must be shared wasm32 with maximum 256MiB"
                );
                memory_type = Some(ty);
            }
        }
        let memory = SharedMemory::new(
            &self.inner.core_engine,
            memory_type.context("core has no shared memory import")?,
        )
        .map_err(error)?;
        let execution = Arc::new(Execution {
            handler_instances: Mutex::new(Vec::new()),
            handler_instance_reuses: AtomicU64::new(0),
            handler_round_trip_us: Mutex::new(Vec::new()),
            handlers_next: AtomicU64::new(1),
            continuations: Mutex::new(HashMap::new()),
            handler_failure: Mutex::new(None),
            runtime: self.clone(),
            module,
            memory,
            effects: effects
                .delegated(hash, definition.allowed_effects.as_deref())
                .with_declared(definition.sig.effects.declared.as_deref()),
            pure,
            jobs: Mutex::new(HashMap::new()),
            tasks: Mutex::new(Vec::new()),
            job_count: AtomicU64::new(0),
            initialization: AsyncMutex::new(()),
            permits: Arc::new(Semaphore::new(8)),
            cancelled: AtomicBool::new(false),
            cancellation: Notify::new(),
            deadline: Instant::now() + Duration::from_secs(EXECUTION_SECONDS),
        });
        let mut cleanup = Cleanup {
            execution: Some(execution.clone()),
        };
        let invocation = entry.prepare()?;
        let task_execution = execution.clone();
        let task_scope = scope.to_owned();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        execution.schedule(async move {
            let result = task_execution.invoke(task_scope, invocation).await;
            let _ = sender.send(result);
        })?;
        let result = receiver
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("shared root task ended without a result")));
        execution.cancel();
        execution.drain().await;
        if let Some(trace) = &execution.effects.trace {
            trace.finish_scope(scope);
        }
        cleanup.execution.take();
        *self.inner.handler_round_trip_us.lock().unwrap() = HandlerMeasurements {
            scope: scope.to_owned(),
            samples: std::mem::take(&mut *execution.handler_round_trip_us.lock().unwrap()),
        };
        let result = result.map_err(|error| {
            let failure = execution
                .handler_failure
                .lock()
                .unwrap()
                .clone()
                .or_else(|| {
                    execution.jobs.lock().unwrap().values().find_map(|job| {
                        // Detached failures belong to their join handles, not to an
                        // unrelated failure of the definition's entry.
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
                });
            match failure {
                Some(failure) => error.context(failure),
                None => error.context(format!("shared execution {scope}")),
            }
        });
        Ok(EncodedCall {
            output: result?,
            timing: RuntimeTiming {
                component_hash: artifact,
                total_ms: elapsed_ms(start),
                run_ms: elapsed_ms(start),
                ..Default::default()
            },
        })
    }
}
