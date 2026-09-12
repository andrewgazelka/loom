//! Actual SDK cancellation witness. The fixture reports its borrowed AtomicU32
//! through a real root effect before continuously updating it in a handler.
use super::*;
use std::sync::atomic::AtomicU32;

/// Safety: the address must satisfy the trusted fixture contract below.
unsafe fn counter(memory: &SharedMemory, address: u32) -> Result<&AtomicU32> {
    let start = address as usize;
    anyhow::ensure!(
        start
            .checked_add(4)
            .is_some_and(|end| end <= memory.data_size()),
        "borrow witness outside shared memory"
    );
    let pointer = memory.data()[start].get().cast::<u32>();
    anyhow::ensure!(
        (pointer as usize).is_multiple_of(std::mem::align_of::<AtomicU32>()),
        "unaligned borrow witness"
    );
    // The production fixture reports a live AtomicU32. SharedMemory stays owned
    // throughout this reference's lifetime. Match the guest's atomic width;
    // never race a byte copy against its 32-bit atomic writes.
    Ok(unsafe { AtomicU32::from_ptr(pointer) })
}

impl Runtime {
    /// Run only the repository-owned `effects-fixtures/cancellation.rs` control.
    ///
    /// # Safety
    /// `bytes` must be the trusted compiler output of that exact fixture. Its
    /// reported address must name a live, aligned AtomicU32 accessed solely
    /// with 32-bit atomic operations until the execution is drained. An
    /// arbitrary Wasm module can violate those requirements; an artifact hash
    /// alone establishes identity, not this atomic-access/lifetime contract.
    pub async unsafe fn verify_borrowed_handler_cancellation(&self, bytes: &[u8]) -> Result<Value> {
        anyhow::ensure!(
            loom_proto::component_protocol::is_core_current(bytes),
            "cancellation control requires current core ABI"
        );
        let module_hash = blake3::hash(bytes).to_hex().to_string();
        let module = Module::new(&self.inner.core_engine, bytes).map_err(error)?;
        let mut memory_type = None;
        for import in module.imports() {
            if let ExternType::Memory(ty) = import.ty() {
                anyhow::ensure!(
                    import.module() == "env" && import.name() == "memory" && memory_type.is_none(),
                    "control requires exactly env.memory"
                );
                anyhow::ensure!(
                    ty.is_shared()
                        && !ty.is_64()
                        && ty
                            .maximum()
                            .is_some_and(|pages| pages * 65536 <= MAX_MEMORY),
                    "invalid control memory limits"
                );
                memory_type = Some(ty);
            }
        }
        let memory = SharedMemory::new(
            &self.inner.core_engine,
            memory_type.context("missing shared memory")?,
        )
        .map_err(error)?;
        let scope = format!("handler-cancellation:{}", uuid::Uuid::new_v4());
        let trace = trace::ExecutionTrace::fresh(&scope);
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
            effects: EffectContext {
                trace: Some(trace.clone()),
                ..Default::default()
            },
            pure: false,
            jobs: Mutex::new(HashMap::new()),
            tasks: Mutex::new(Vec::new()),
            job_count: AtomicU64::new(0),
            initialization: AsyncMutex::new(()),
            permits: Arc::new(Semaphore::new(8)),
            cancelled: AtomicBool::new(false),
            cancellation: Notify::new(),
            deadline: Instant::now() + Duration::from_secs(10),
        });
        let mut cleanup = Cleanup {
            execution: Some(execution.clone()),
        };
        let child = execution.clone();
        let task_scope = scope.clone();
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        execution.schedule(async move {
            let result = child
                .invoke(
                    task_scope,
                    Invocation::Call {
                        args: encode(&json!([])).unwrap(),
                    },
                )
                .await;
            let _ = sender.send(result);
        })?;
        let address = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                for blob in trace.snapshot(None, false)?.blobs {
                    if blob.kind != loom_proto::TraceBlobKind::Descriptor {
                        continue;
                    }
                    let descriptor: Value =
                        loom_proto::decode(&blob.bytes).map_err(anyhow::Error::msg)?;
                    if descriptor.get("op").and_then(Value::as_str) == Some("now")
                        && let Some(address) = descriptor
                            .get("args")
                            .and_then(|args| args.get("borrow_address"))
                            .and_then(Value::as_u64)
                    {
                        return u32::try_from(address).context("invalid borrow witness address");
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("borrowed callback did not report its address")??;
        // SAFETY: this method's caller supplies the exact trusted fixture;
        // `execution` keeps its backing memory live through all atomic access.
        let borrowed = unsafe { counter(&execution.memory, address)? };
        tokio::time::timeout(Duration::from_secs(3), async {
            let first = borrowed.load(Ordering::SeqCst);
            while borrowed.load(Ordering::SeqCst) == first {
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("callback was not actively updating borrowed parent storage")?;
        anyhow::ensure!(
            execution.handler_instance_reuses.load(Ordering::Relaxed) >= 1,
            "active callback did not reuse the warmed handler cache"
        );
        anyhow::ensure!(
            !execution.continuations.lock().unwrap().is_empty(),
            "no active suspended performer"
        );
        execution.cancel();
        tokio::time::timeout(Duration::from_secs(3), execution.drain())
            .await
            .context("actual guest Store drain timed out")?;
        cleanup.execution.take();
        anyhow::ensure!(
            execution.tasks.lock().unwrap().is_empty()
                && execution.handler_instances.lock().unwrap().is_empty(),
            "guest tasks or cached Stores survived drain"
        );
        anyhow::ensure!(
            Arc::strong_count(&execution) == 1,
            "a guest Store still owns execution after drain"
        );
        match receiver.try_recv() {
            Ok(Err(_)) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {}
            Ok(Ok(_)) => bail!("infinite borrowed callback unexpectedly returned normally"),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                bail!("root completion channel is still pending after drain")
            }
        }
        // Backing SharedMemory remains owned by `execution`. Only now, after
        // actual Store destruction, repurpose the borrowed parent storage.
        let sentinel = 0xcafe_babe;
        borrowed.store(sentinel, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        anyhow::ensure!(
            borrowed.load(Ordering::SeqCst) == sentinel,
            "callback wrote parent storage after drain"
        );
        Ok(
            json!({"scope": scope, "module_hash": module_hash, "pass": true,
            "active_before_cancel": true, "drained": true, "parent_storage_reused_after_drain": true,
            "stores_after_drain": 0, "handler_instance_reuses": execution.handler_instance_reuses.load(Ordering::Relaxed)}),
        )
    }
}
