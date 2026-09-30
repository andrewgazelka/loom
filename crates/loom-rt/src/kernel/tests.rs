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
        &["echo", "sum", "blob_len", "boom"]
    }
    fn call(&self, context: &KernelContext<'_>, op: &str, args: &[&[u8]]) -> Result<Vec<u8>, String> {
        match op {
            "echo" => Ok(args.concat()),
            "sum" => Ok(args.iter().flat_map(|part| part.iter()).map(|b| *b as u64).sum::<u64>().to_le_bytes().to_vec()),
            "blob_len" => {
                let handle: Handle = args
                    .first()
                    .and_then(|part| (*part).try_into().ok())
                    .ok_or("blob_len takes one 32-byte handle")?;
                let bytes = context.blob(&handle)?.ok_or("unknown handle")?;
                Ok((bytes.len() as u64).to_le_bytes().to_vec())
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
    assert_eq!(runtime.call_kernel("test.echo", &[b"ab", b"", b"cd"]).unwrap(), b"abcd");
    assert_eq!(
        u64::from_le_bytes(runtime.call_kernel("test.sum", &[&[1, 2], &[3]]).unwrap().try_into().unwrap()),
        6
    );
}

#[test]
fn unknown_ops_errors_and_panics_reach_the_caller_as_errors_not_crashes() {
    let runtime = runtime();
    assert!(runtime.call_kernel("test.nope", &[]).unwrap_err().contains("no kernel op"));
    assert!(runtime.call_kernel("other.echo", &[]).is_err());
    assert!(runtime.call_kernel("test.boom", &[]).unwrap_err().contains("panicked"));
    // The runtime is intact after a kernel bug.
    assert!(runtime.call_kernel("test.echo", &[b"x"]).is_ok());
}

#[test]
fn put_returns_the_content_hash_and_a_kernel_can_read_the_bytes_back_by_it() {
    let runtime = runtime();
    let bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let handle = runtime.call_kernel("loom.put", &[&bytes[..40_000], &bytes[40_000..]]).unwrap();
    assert_eq!(handle, blake3::hash(&bytes).as_bytes().to_vec(), "the handle is the BLAKE3 hash of the bytes");
    // Storing the same bytes again names the same handle.
    assert_eq!(runtime.call_kernel("loom.put", &[&bytes]).unwrap(), handle);
    let length = runtime.call_kernel("test.blob_len", &[&handle]).unwrap();
    assert_eq!(u64::from_le_bytes(length.try_into().unwrap()), 100_000);
    // A hash this host never stored is an error, not a crash.
    let unknown = [9u8; 32];
    assert!(runtime.call_kernel("test.blob_len", &[&unknown]).unwrap_err().contains("unknown handle"));
}

#[test]
fn registration_refuses_reserved_malformed_and_duplicate_families_and_versions_move_the_fingerprint() {
    let runtime = Runtime::new(Store::memory().unwrap()).unwrap();
    let empty = runtime.kernel_fingerprint();
    runtime.register_kernel(Arc::new(Test)).unwrap();
    let with_test = runtime.kernel_fingerprint();
    assert_ne!(empty, with_test);
    assert!(runtime.register_kernel(Arc::new(Test)).is_err(), "a family registers once");

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
        assert!(runtime.register_kernel(Arc::new(Reserved(family))).is_err(), "{family:?}");
    }
    assert_eq!(runtime.kernel_fingerprint(), with_test, "refused registrations change nothing");

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
    assert_ne!(bumped.kernel_fingerprint(), with_test, "the same family at another version differs");
}
