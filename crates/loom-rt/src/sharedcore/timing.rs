//! Native diagnostic runner. Reuses the production guest Store/instance path;
//! the clock brackets a complete guest export, including its typed codec work.
use super::*;

impl Runtime {
    /// Time a checked fixture whose no-argument main installs one handler,
    /// performs exactly one effect, and returns 1. Module compilation, root
    /// setup and host result inspection are outside the measured interval.
    /// Each sample includes handler installation/removal and guest entry codec,
    /// so this is an upper bound on a single complete effect round trip.
    pub async fn benchmark_handler_module(&self, bytes: &[u8]) -> Result<Value> {
        anyhow::ensure!(
            loom_proto::core_protocol::is_current(bytes),
            "benchmark requires a current admitted core artifact"
        );
        let module_hash = blake3::hash(bytes).to_hex().to_string();
        let module = Module::new(&self.inner.core_engine, bytes).map_err(error)?;
        let mut memory_type = None;
        for import in module.imports() {
            if let ExternType::Memory(ty) = import.ty() {
                anyhow::ensure!(
                    import.module() == "env" && import.name() == "memory" && memory_type.is_none(),
                    "benchmark core must import exactly env.memory"
                );
                anyhow::ensure!(
                    ty.is_shared()
                        && !ty.is_64()
                        && ty
                            .maximum()
                            .is_some_and(|pages| pages * 65536 <= MAX_MEMORY),
                    "invalid benchmark shared memory limits"
                );
                memory_type = Some(ty);
            }
        }
        let memory = SharedMemory::new(
            &self.inner.core_engine,
            memory_type.context("benchmark core has no shared memory")?,
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
            effects: EffectContext::default(),
            pure: true,
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
        let scope = format!("handler-timing:{}", uuid::Uuid::new_v4());
        let task_scope = scope.clone();
        let child = execution.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        execution.schedule(async move {
            let result: Result<Vec<f64>> = async {
                let mut running = child.instantiate(task_scope, None, Vec::new()).await?;
                let input = running.input_encoded(&encode(&json!([]))?).await?;
                let call = running
                    .instance
                    .get_typed_func::<(i32, i32), i64>(&mut running.store, "loom_call_main")
                    .map_err(error)?;
                let free = running
                    .instance
                    .get_typed_func::<(i32, i32, i32), ()>(&mut running.store, "loom_dealloc")
                    .map_err(error)?;
                let mut samples = Vec::with_capacity(10_000);
                for index in 0..10_100 {
                    if index == 100 {
                        child.handler_round_trip_us.lock().unwrap().clear();
                    }
                    let start = Instant::now();
                    let packed = call
                        .call_async(&mut running.store, (input.pointer, input.length))
                        .await
                        .map_err(error)? as u64;
                    let elapsed = start.elapsed().as_secs_f64() * 1_000_000.0;
                    let bytes = copy_out(&child.memory, packed as u32, (packed >> 32) as u32)?;
                    let response: Value = loom_proto::decode(&bytes).map_err(anyhow::Error::msg)?;
                    anyhow::ensure!(
                        response == json!({"ok": 1}),
                        "benchmark fixture returned {response}, expected 1"
                    );
                    anyhow::ensure!(
                        running.store.data().handlers.is_empty(),
                        "benchmark fixture leaked handler frames"
                    );
                    anyhow::ensure!(
                        child.continuations.lock().unwrap().is_empty(),
                        "benchmark fixture leaked continuations"
                    );
                    free.call_async(
                        &mut running.store,
                        (packed as u32 as i32, (packed >> 32) as i32, 1),
                    )
                    .await
                    .map_err(error)?;
                    if index >= 100 {
                        samples.push(elapsed);
                    }
                }
                anyhow::ensure!(
                    child.handler_round_trip_us.lock().unwrap().len() == 10_000,
                    "benchmark fixture must perform exactly one handled effect per sample"
                );
                anyhow::ensure!(
                    child.jobs.lock().unwrap().is_empty(),
                    "benchmark fixture must not spawn jobs"
                );
                anyhow::ensure!(
                    child.handler_instance_reuses.load(Ordering::Relaxed) >= 10_000,
                    "handler cache did not serve the measured workload"
                );
                free.call_async(&mut running.store, (input.pointer, input.length, 1))
                    .await
                    .map_err(error)?;
                Ok(samples)
            }
            .await;
            let _ = sender.send(result);
        })?;
        let result = receiver
            .await
            .context("benchmark guest task ended without result");
        execution.cancel();
        execution.drain().await;
        cleanup.execution.take();
        let mut samples = result??;
        samples.sort_by(f64::total_cmp);
        Ok(
            json!({"scope": scope, "samples": samples.len(), "warmup_samples": 100,
            "median": samples[samples.len()/2], "p99": samples[samples.len()*99/100],
            "unit": "us", "module_hash": module_hash,
            "handler_instance_reuses": execution.handler_instance_reuses.load(Ordering::Relaxed),
            "measurement": "complete guest export upper bound: handler installation, typed perform codec, handler instance dispatch, resume and handler removal",
            "target_median_us": 20, "pass": samples[samples.len()/2] < 20.0}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    #[ignore = "requires LOOM_HANDLER_BENCH_MODULE built from scripts/bench/effects-fixtures/timing.rs"]
    async fn actual_sdk_full_handler_round_trip() -> Result<()> {
        let bytes = std::fs::read(std::env::var("LOOM_HANDLER_BENCH_MODULE")?)?;
        let runtime = Runtime::new(Store::memory()?)?;
        let result = runtime.benchmark_handler_module(&bytes).await?;
        println!("{result}");
        anyhow::ensure!(result["samples"] == 10_000, "incomplete timing workload");
        anyhow::ensure!(
            result["pass"] == true,
            "complete round-trip median is not under 20us"
        );
        Ok(())
    }
}
