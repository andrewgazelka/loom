use std::path::Path;
use std::process::{Command, Output};

fn compile(directory: &Path, cache: Option<&Path>) -> Output {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(
        directory.join("input.rs"),
        "pub fn value(x: u64) -> u64 { x ^ 17 }",
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hash-rustc"));
    command
        .current_dir(directory)
        .args([
            "input.rs",
            "--crate-type=rlib",
            "-Copt-level=2",
            "-Clto=off",
            "-Cembed-bitcode=no",
        ])
        .env_remove("LOOM_ITEM_HASHES")
        .env_remove("LOOM_ITEM_PREIMAGES")
        .env_remove("LOOM_OBJECT_CACHE")
        .env_remove("LOOM_OBJECT_CACHE_STATS")
        .env_remove("LOOM_OBJECT_CACHE_TIMINGS")
        // If the disabled path accidentally enters publication it must also fail.
        .env("LOOM_OBJECT_CACHE_FAIL_PUBLISH", "1")
        .env("LOOM_OBJECT_CACHE_CALLS", "1");
    if let Some(cache) = cache {
        command
            .env("LOOM_OBJECT_CACHE", cache)
            .env_remove("LOOM_OBJECT_CACHE_FAIL_PUBLISH");
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn unset_cache_performs_no_hashing_or_cache_io() {
    let root = tempfile::tempdir().unwrap();
    let disabled = compile(&root.path().join("disabled"), None);
    let stderr = String::from_utf8(disabled.stderr).unwrap();
    assert!(
        stderr
            .lines()
            .any(|line| line
                == "object-cache-calls: hashing=0 lookup=0 store=0 publish=0 object_copy=0"),
        "{stderr}"
    );
    // Positive control proves these counters observe the actual store and hasher,
    // rather than asserting that an unused or disconnected instrument reads zero.
    let enabled = compile(
        &root.path().join("enabled"),
        Some(&root.path().join("cache")),
    );
    let stderr = String::from_utf8(enabled.stderr).unwrap();
    let line = stderr
        .lines()
        .find(|line| line.starts_with("object-cache-calls:"))
        .unwrap();
    for value in line.split_whitespace().skip(1) {
        assert!(
            value.split_once('=').unwrap().1.parse::<u64>().unwrap() > 0,
            "{line}"
        );
    }
    assert!(
        std::fs::read_dir(root.path().join("cache"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|extension| extension == "o"))
    );
}

#[test]
fn entirely_unsupported_crate_bypasses_before_hir_encoding() {
    let root = tempfile::tempdir().unwrap();
    // HIR and mono hashing support this shape; object admission still rejects the pointer body.
    std::fs::write(root.path().join("input.rs"), "pub trait Invocation { type Args; fn arguments(args: Self::Args) -> Vec<u8>; } #[no_mangle] pub fn entry(input: *mut u8) -> *mut u8 { input }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
        .current_dir(root.path())
        .args([
            "input.rs",
            "--crate-type=rlib",
            "-Copt-level=2",
            "-Clto=off",
            "-Cembed-bitcode=no",
            "-Zembed-metadata=no",
            "-Clinker=/usr/bin/cc",
        ])
        .env_remove("LOOM_ITEM_HASHES")
        .env_remove("LOOM_ITEM_PREIMAGES")
        .env_remove("LOOM_OBJECT_CACHE_FAIL_PUBLISH")
        .env("LOOM_OBJECT_CACHE", root.path().join("cache"))
        .env("LOOM_OBJECT_CACHE_CALLS", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("requires layout, allocation, or drop identity"),
        "{stderr}"
    );
    assert!(
        stderr.contains("lookup=0 store=0 publish=0 object_copy=0"),
        "{stderr}"
    );
    assert!(!root.path().join("cache").exists());
}
