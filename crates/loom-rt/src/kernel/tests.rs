use super::*;

struct Test;
impl HostKernel for Test {
    fn family(&self) -> &str {
        "test"
    }
    fn version(&self) -> u32 {
        3
    }
    fn ops(&self) -> &[&'static str] {
        &["echo", "sum", "blob_len", "boom", "map_sum", "double"]
    }
    fn call(
        &self,
        context: &KernelContext<'_>,
        op: &str,
        args: &[&[u8]],
    ) -> Result<Vec<u8>, String> {
        match op {
            "echo" => Ok(args.concat()),
            "sum" => Ok(args
                .iter()
                .flat_map(|part| part.iter())
                .map(|b| *b as u64)
                .sum::<u64>()
                .to_le_bytes()
                .to_vec()),
            "blob_len" => {
                let handle: Handle = args
                    .first()
                    .and_then(|part| (*part).try_into().ok())
                    .ok_or("blob_len takes one 32-byte handle")?;
                let bytes = context.blob(&handle)?.ok_or("unknown handle")?;
                Ok((bytes.len() as u64).to_le_bytes().to_vec())
            }
            // Sum the bytes of a blob in place, through the mapping, and say whether it was file-backed.
            "map_sum" => {
                let handle: Handle = args
                    .first()
                    .and_then(|part| (*part).try_into().ok())
                    .ok_or("map_sum takes one 32-byte handle")?;
                let mapped = context.map(&handle)?.ok_or("unknown handle")?;
                let sum: u64 = mapped.iter().map(|b| *b as u64).sum();
                let mut out = sum.to_le_bytes().to_vec();
                out.push(u8::from(mapped.is_file_backed()));
                Ok(out)
            }
            // Return a large result as a handle instead of bytes.
            "double" => {
                let handle: Handle = args
                    .first()
                    .and_then(|part| (*part).try_into().ok())
                    .ok_or("double takes one 32-byte handle")?;
                let mapped = context.map(&handle)?.ok_or("unknown handle")?;
                let doubled: Vec<u8> = mapped.iter().map(|b| b.wrapping_mul(2)).collect();
                Ok(context.put(&doubled)?.to_vec())
            }
            "boom" => panic!("kernel bug"),
            other => Err(format!("no op {other}")),
        }
    }
}

fn runtime() -> Runtime {
    let runtime = Runtime::new(Store::memory().unwrap()).unwrap();
    runtime.register_kernel(Arc::new(Test)).unwrap();
    runtime
}

#[test]
fn a_registered_op_runs_on_its_gather_list_in_order() {
    let runtime = runtime();
    assert_eq!(
        runtime
            .call_kernel("test.echo", &[b"ab", b"", b"cd"])
            .unwrap(),
        b"abcd"
    );
    assert_eq!(
        u64::from_le_bytes(
            runtime
                .call_kernel("test.sum", &[&[1, 2], &[3]])
                .unwrap()
                .try_into()
                .unwrap()
        ),
        6
    );
}

#[test]
fn unknown_ops_errors_and_panics_reach_the_caller_as_errors_not_crashes() {
    let runtime = runtime();
    assert!(
        runtime
            .call_kernel("test.nope", &[])
            .unwrap_err()
            .contains("no kernel op")
    );
    assert!(runtime.call_kernel("other.echo", &[]).is_err());
    assert!(
        runtime
            .call_kernel("test.boom", &[])
            .unwrap_err()
            .contains("panicked")
    );
    // The runtime is intact after a kernel bug.
    assert!(runtime.call_kernel("test.echo", &[b"x"]).is_ok());
}

#[test]
fn put_returns_the_content_hash_and_a_kernel_can_read_the_bytes_back_by_it() {
    let runtime = runtime();
    let bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let handle = runtime
        .call_kernel("loom.put", &[&bytes[..40_000], &bytes[40_000..]])
        .unwrap();
    assert_eq!(
        handle,
        blake3::hash(&bytes).as_bytes().to_vec(),
        "the handle is the BLAKE3 hash of the bytes"
    );
    // Storing the same bytes again names the same handle.
    assert_eq!(runtime.call_kernel("loom.put", &[&bytes]).unwrap(), handle);
    let length = runtime.call_kernel("test.blob_len", &[&handle]).unwrap();
    assert_eq!(u64::from_le_bytes(length.try_into().unwrap()), 100_000);
    // A hash this host never stored is an error, not a crash.
    let unknown = [9u8; 32];
    assert!(
        runtime
            .call_kernel("test.blob_len", &[&unknown])
            .unwrap_err()
            .contains("unknown handle")
    );
}

#[test]
fn get_returns_the_bytes_a_put_stored_and_nothing_else() {
    let runtime = runtime();
    let bytes: Vec<u8> = (0..70_000u32).map(|i| (i % 253) as u8).collect();
    let handle = runtime.call_kernel("loom.put", &[&bytes]).unwrap();
    assert_eq!(runtime.call_kernel("loom.get", &[&handle]).unwrap(), bytes);
    assert!(runtime.call_kernel("loom.get", &[&[5u8; 32]]).unwrap_err().contains("unknown handle"));
    assert!(runtime.call_kernel("loom.get", &[b"short"]).unwrap_err().contains("32-byte"));
    assert!(runtime.call_kernel("loom.get", &[]).unwrap_err().contains("32-byte"));
    // A hash of an object stored as something else is unknown, not readable.
    let other = runtime.inner.store.put("component", b"not a kernel blob").unwrap();
    let other: Handle = unhex(&other).unwrap();
    assert!(runtime.call_kernel("loom.get", &[&other]).unwrap_err().contains("unknown handle"));
}

#[test]
fn registration_refuses_reserved_malformed_and_duplicate_families_and_versions_move_the_fingerprint()
 {
    let runtime = Runtime::new(Store::memory().unwrap()).unwrap();
    let empty = runtime.kernel_fingerprint();
    runtime.register_kernel(Arc::new(Test)).unwrap();
    let with_test = runtime.kernel_fingerprint();
    assert_ne!(empty, with_test);
    assert!(
        runtime.register_kernel(Arc::new(Test)).is_err(),
        "a family registers once"
    );

    struct Reserved(&'static str);
    impl HostKernel for Reserved {
        fn family(&self) -> &str {
            self.0
        }
        fn version(&self) -> u32 {
            1
        }
        fn ops(&self) -> &[&'static str] {
            &[]
        }
        fn call(&self, _: &KernelContext<'_>, _: &str, _: &[&[u8]]) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }
    }
    for family in ["loom", "", "a.b"] {
        assert!(
            runtime.register_kernel(Arc::new(Reserved(family))).is_err(),
            "{family:?}"
        );
    }
    assert_eq!(
        runtime.kernel_fingerprint(),
        with_test,
        "refused registrations change nothing"
    );

    let bumped = Runtime::new(Store::memory().unwrap()).unwrap();
    struct V4;
    impl HostKernel for V4 {
        fn family(&self) -> &str {
            "test"
        }
        fn version(&self) -> u32 {
            4
        }
        fn ops(&self) -> &[&'static str] {
            &[]
        }
        fn call(&self, _: &KernelContext<'_>, _: &str, _: &[&[u8]]) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }
    }
    bumped.register_kernel(Arc::new(V4)).unwrap();
    assert_ne!(
        bumped.kernel_fingerprint(),
        with_test,
        "the same family at another version differs"
    );
}

#[test]
fn a_handle_names_only_what_put_stored_not_other_objects_in_the_store() {
    let runtime = runtime();
    let other = runtime
        .inner
        .store
        .put("definition", b"secret source")
        .unwrap();
    let handle = unhex(&other).unwrap();
    let error = runtime
        .call_kernel("test.blob_len", &[&handle])
        .unwrap_err();
    assert_eq!(
        error, "unknown handle",
        "an object of another kind looks like a missing one"
    );
    let put = runtime.call_kernel("loom.put", &[b"mesh bytes"]).unwrap();
    assert_eq!(
        u64::from_le_bytes(
            runtime
                .call_kernel("test.blob_len", &[&put])
                .unwrap()
                .try_into()
                .unwrap()
        ),
        10
    );
}

#[test]
fn a_failed_call_moves_the_failure_counter_and_a_successful_one_does_not() {
    let runtime = runtime();
    let before = runtime.kernel_failures();
    runtime.call_kernel("test.echo", &[b"x"]).unwrap();
    assert_eq!(runtime.kernel_failures(), before);
    runtime.call_kernel("nope.op", &[]).unwrap_err();
    runtime.call_kernel("test.boom", &[]).unwrap_err();
    assert_eq!(runtime.kernel_failures(), before + 2);
}

#[tokio::test]
async fn blocking_calls_run_off_the_caller_thread_and_return_the_same_bytes() {
    let runtime = runtime();
    let reply = runtime
        .call_kernel_blocking("test.echo".into(), vec![b"ab".to_vec(), b"cd".to_vec()])
        .await;
    assert_eq!(reply.unwrap(), b"abcd");
    let many: Vec<_> = (0..64)
        .map(|i| {
            let runtime = runtime.clone();
            tokio::spawn(async move {
                runtime
                    .call_kernel_blocking("test.echo".into(), vec![vec![i as u8]])
                    .await
            })
        })
        .collect();
    for (i, task) in many.into_iter().enumerate() {
        assert_eq!(task.await.unwrap().unwrap(), vec![i as u8]);
    }
}

#[test]
fn a_kernel_reads_a_large_blob_in_place_and_returns_a_large_result_as_a_handle() {
    // A file-backed store, so a blob of 1 MiB and up is an object file the kernel maps.
    let directory = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(Store::open(directory.path().join("store.db")).unwrap()).unwrap();
    runtime.register_kernel(Arc::new(Test)).unwrap();
    let big: Vec<u8> = (0..(2 << 20)).map(|i| (i % 251) as u8).collect();
    let handle = runtime.call_kernel("loom.put", &[&big]).unwrap();
    let out = runtime.call_kernel("test.map_sum", &[&handle]).unwrap();
    let expected: u64 = big.iter().map(|b| *b as u64).sum();
    assert_eq!(u64::from_le_bytes(out[..8].try_into().unwrap()), expected);
    assert_eq!(out[8], 1, "a 2 MiB blob is mapped from its object file, not copied");
    // A small blob is an inline copy.
    let small = runtime.call_kernel("loom.put", &[&big[..1000]]).unwrap();
    assert_eq!(runtime.call_kernel("test.map_sum", &[&small]).unwrap()[8], 0);

    // The kernel returns a 32-byte handle; the embedder maps the result without copying it out of a reply.
    let doubled = runtime.call_kernel("test.double", &[&handle]).unwrap();
    assert_eq!(doubled.len(), 32);
    let doubled: Handle = doubled.try_into().unwrap();
    let mapped = runtime.map_blob(&doubled).unwrap().expect("the result blob");
    assert!(mapped.is_file_backed());
    assert_eq!(mapped.len(), big.len());
    assert_eq!(mapped[5], big[5].wrapping_mul(2));
    // Only blobs written through `loom.put` are reachable this way: not an unknown hash, and not an object
    // another part of the system stored under a different kind.
    assert!(runtime.map_blob(&[7u8; 32]).unwrap().is_none());
    let component = vec![9u8; 2 << 20];
    let other = runtime.inner.store.put("component", &component).unwrap();
    let other: Handle = unhex(&other).unwrap();
    assert!(runtime.map_blob(&other).unwrap().is_none());
    // A reference with the right hash but a wrong length is refused.
    let reference = loom_proto::StoreRef { hash: doubled, len: big.len() as u64 };
    assert!(runtime.map_ref(&reference).unwrap().is_some());
    let lying = loom_proto::StoreRef { len: 1, ..reference };
    assert!(runtime.map_ref(&lying).is_err());
}
