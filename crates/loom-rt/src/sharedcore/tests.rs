use super::*;
#[test]
fn empty_tls_exports_need_no_alignment_but_nonempty_tls_is_checked() {
    assert_eq!(tls_alignment(0, 0).unwrap(), 1);
    assert_eq!(tls_alignment(16, 16).unwrap(), 16);
    assert!(tls_alignment(16, 0).is_err());
    assert!(tls_alignment(16, 3).is_err());
    assert!(tls_alignment(16, 131072).is_err());
}
#[test]
fn shared_copy_checks_ranges_and_preserves_concurrent_atomic_access() {
    let (engine, _cache) = engine(Store::memory().unwrap()).unwrap();
    let memory = SharedMemory::new(&engine, wasmtime::MemoryType::shared(1, 2)).unwrap();
    copy_in(&memory, 32, &[1, 2, 3]).unwrap();
    assert_eq!(copy_out(&memory, 32, 3).unwrap(), vec![1, 2, 3]);
    assert!(copy_out(&memory, u32::MAX, 2).is_err());
    assert!(copy_in(&memory, 65535, &[1, 2]).is_err());
    let worker_memory = memory.clone();
    let worker = std::thread::spawn(move || {
        for _ in 0..1000 {
            copy_in(&worker_memory, 32, &[4, 5, 6]).unwrap();
        }
    });
    for _ in 0..1000 {
        assert_eq!(copy_out(&memory, 32, 3).unwrap().len(), 3);
    }
    worker.join().unwrap();
    assert_eq!(copy_out(&memory, 32, 3).unwrap(), vec![4, 5, 6]);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_drains_saturated_guest_workers_before_releasing_execution() -> Result<()> {
    let runtime = Runtime::new(Store::memory()?)?;
    let module = Module::new(
        &runtime.inner.core_engine,
        r#"(module
            (import "env" "memory" (memory 1 1 shared))
            (global (export "__stack_pointer") (mut i32) (i32.const 65536))
            (global (export "__loom_stack_low") (mut i32) (i32.const 1024))
            (global (export "__loom_stack_high") (mut i32) (i32.const 65536))
            (func (export "spin") (param i32)
                local.get 0 i32.const 1 i32.atomic.store8
                (loop br 0)))"#,
    )
    .map_err(error)?;
    let memory = SharedMemory::new(
        &runtime.inner.core_engine,
        wasmtime::MemoryType::shared(1, 1),
    )
    .map_err(error)?;
    let execution = Arc::new(Execution {
        handler_instances: Mutex::new(Vec::new()),
        handler_instance_reuses: AtomicU64::new(0),
        handler_round_trip_us: Mutex::new(Vec::new()),
        handlers_next: AtomicU64::new(1),
        continuations: Mutex::new(HashMap::new()),
        handler_failure: Mutex::new(None),
        runtime,
        module,
        memory,
        effects: EffectContext::default(),
        pure: false,
        jobs: Mutex::new(HashMap::new()),
        tasks: Mutex::new(Vec::new()),
        job_count: AtomicU64::new(8),
        initialization: AsyncMutex::new(()),
        permits: Arc::new(Semaphore::new(8)),
        cancelled: AtomicBool::new(false),
        cancellation: Notify::new(),
        deadline: Instant::now() + Duration::from_secs(2),
    });
    for index in 0..8 {
        let child = execution.clone();
        execution.schedule(async move {
            let mut guest = child
                .instantiate(format!("root:{index}"), None, Vec::new())
                .await
                .unwrap();
            let spin = guest
                .instance
                .get_typed_func::<i32, ()>(&mut guest.store, "spin")
                .unwrap();
            let _ = spin.call_async(&mut guest.store, index).await;
        })?;
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while copy_out(&execution.memory, 0, 8).unwrap() != [1; 8] {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    execution.cancel();
    tokio::time::timeout(Duration::from_secs(1), execution.drain()).await?;
    assert!(execution.tasks.lock().unwrap().is_empty());
    assert_eq!(
        Arc::strong_count(&execution),
        1,
        "guest Store still owns execution after drain"
    );
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires LOOM_SHARED_TEST_MODULE compiled production SDK module"]
async fn actual_sdk_module_rendezvous() -> Result<()> {
    let bytes = std::fs::read(std::env::var("LOOM_SHARED_TEST_MODULE")?)?;
    let runtime = Runtime::new(Store::memory()?)?;
    let module = Module::new(&runtime.inner.core_engine, bytes).map_err(error)?;
    let memory_type = module
        .imports()
        .find_map(|import| match import.ty() {
            ExternType::Memory(memory) => Some(memory),
            _ => None,
        })
        .context("memory")?;
    let memory = SharedMemory::new(&runtime.inner.core_engine, memory_type).map_err(error)?;
    let execution = Arc::new(Execution {
        handler_instances: Mutex::new(Vec::new()),
        handler_instance_reuses: AtomicU64::new(0),
        handler_round_trip_us: Mutex::new(Vec::new()),
        handlers_next: AtomicU64::new(1),
        continuations: Mutex::new(HashMap::new()),
        handler_failure: Mutex::new(None),
        runtime,
        module,
        memory,
        effects: EffectContext::default(),
        pure: false,
        jobs: Mutex::new(HashMap::new()),
        tasks: Mutex::new(Vec::new()),
        job_count: AtomicU64::new(0),
        initialization: AsyncMutex::new(()),
        permits: Arc::new(Semaphore::new(8)),
        cancelled: AtomicBool::new(false),
        cancellation: Notify::new(),
        deadline: Instant::now() + Duration::from_secs(3),
    });
    let mut cleanup = Cleanup {
        execution: Some(execution.clone()),
    };
    let child = execution.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    execution.schedule(async move {
        let result = child
            .invoke(
                "diagnostic".into(),
                Invocation::Call {
                    export: "loom_call_main".into(),
                    args: encode(&json!([])).unwrap(),
                },
            )
            .await;
        let _ = sender.send(result);
    })?;
    let result = receiver.await?;
    execution.cancel();
    execution.drain().await;
    cleanup.execution.take();
    assert_eq!(result?.decode()?, json!([10, 24, 2]));
    Ok(())
}
#[test]
fn separate_execution_memories_do_not_alias() {
    let (engine, _cache) = engine(Store::memory().unwrap()).unwrap();
    let first = SharedMemory::new(&engine, wasmtime::MemoryType::shared(1, 1)).unwrap();
    let second = SharedMemory::new(&engine, wasmtime::MemoryType::shared(1, 1)).unwrap();
    copy_in(&first, 64, &[42]).unwrap();
    assert_eq!(copy_out(&second, 64, 1).unwrap(), vec![0]);
}
