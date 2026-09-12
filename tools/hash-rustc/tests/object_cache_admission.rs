use std::path::Path;
use std::process::{Command, Output};

#[derive(Debug)]
struct Stats {
    cgus: usize,
    hits: usize,
    misses: usize,
}

fn stats(output: &Output) -> Stats {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let lines: Vec<_> = stderr
        .lines()
        .filter(|line| line.starts_with("object-cache: cgus="))
        .collect();
    assert_eq!(lines.len(), 1, "{stderr}");
    let values: std::collections::BTreeMap<_, _> = lines[0]
        .split_whitespace()
        .skip(1)
        .map(|field| field.split_once('=').unwrap())
        .collect();
    let result = Stats {
        cgus: values["cgus"].parse().unwrap(),
        hits: values["hits"].parse().unwrap(),
        misses: values["misses"].parse().unwrap(),
    };
    assert!(result.cgus > 0, "{stderr}");
    assert_eq!(result.cgus, result.hits + result.misses, "{stderr}");
    result
}

fn compile(directory: &Path, cache: &Path, source: &str, extra: &[&str]) -> Output {
    compile_named(directory, cache, source, extra, "fixture")
}

fn compile_named(
    directory: &Path,
    cache: &Path,
    source: &str,
    extra: &[&str],
    name: &str,
) -> Output {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(directory.join("fixture.rs"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
        .current_dir(directory)
        .args([
            "fixture.rs",
            "--crate-name",
            name,
            "--crate-type=rlib",
            "--edition=2024",
            "-C",
            "lto=off",
            "-C",
            "embed-bitcode=no",
            "-C",
            "debuginfo=0",
            "-C",
            "codegen-units=2",
            "-C",
            "opt-level=2",
        ])
        .args(extra)
        .env("LOOM_OBJECT_CACHE", cache)
        .env("LOOM_OBJECT_CACHE_STATS", "1")
        .env("LOOM_ITEM_HASHES", directory.join("hashes.json"))
        .env("LOOM_ITEM_PREIMAGES", directory.join("preimages"))
        .env_remove("LOOM_OBJECT_CACHE_FAIL_PUBLISH")
        .output()
        .unwrap()
}

fn run(directory: &Path, assertion: &str) {
    run_named(directory, assertion, "fixture");
}

fn run_named(directory: &Path, assertion: &str, name: &str) {
    std::fs::write(
        directory.join("main.rs"),
        format!("fn main() {{ {assertion} }}"),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
        .current_dir(directory)
        .args([
            "main.rs",
            "--edition=2024",
            &format!("--extern=fixture=lib{name}.rlib"),
            "-o",
            "runner",
        ])
        .env_remove("LOOM_OBJECT_CACHE")
        .env_remove("LOOM_OBJECT_CACHE_STATS")
        .env_remove("LOOM_ITEM_HASHES")
        .env_remove("LOOM_ITEM_PREIMAGES")
        .env_remove("LOOM_OBJECT_CACHE_FAIL_PUBLISH")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new(directory.join("runner")).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn unsupported_flag(flags: &[&str], reason: &str) {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let source = "#[inline(never)] pub fn entry(x: u64) -> u64 { x ^ 17 }";
    let baseline = stats(&compile(&root.path().join("base"), &cache, source, &[]));
    assert_eq!(baseline.hits, 0);
    for label in ["first", "second"] {
        let directory = root.path().join(label);
        let output = compile(&directory, &cache, source, flags);
        let result = stats(&output);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("object-cache: bypass") && stderr.contains(reason),
            "{stderr}"
        );
        assert_eq!(result.hits, 0, "{stderr}");
        assert_eq!(result.misses, result.cgus);
        run(
            &directory,
            "assert_eq!(fixture::entry(std::hint::black_box(42)), 42 ^ 17);",
        );
    }
}

#[test]
fn unsupported_codegen_flag_bypasses_with_reason() {
    unsupported_flag(&["-Ctarget-cpu=generic"], "target-cpu");
}

#[test]
fn unsupported_unstable_flag_bypasses_with_reason() {
    unsupported_flag(&["-Zmir-opt-level=1"], "mir-opt-level");
}

#[test]
fn explicitly_supplied_unlisted_default_bypasses_with_reason() {
    unsupported_flag(&["-Csave-temps=no"], "save-temps");
}

#[test]
fn inline_attribute_changes_object_identity() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let source = "#[inline(never)] pub fn entry(x: u64) -> u64 { x ^ 17 }";
    stats(&compile(&root.path().join("first"), &cache, source, &[]));
    let directory = root.path().join("changed");
    let changed = stats(&compile(
        &directory,
        &cache,
        &source.replace("#[inline(never)] ", ""),
        &[],
    ));
    assert_eq!(
        changed.hits, 0,
        "changing codegen attributes must not restore an old object: {changed:?}"
    );
    run(
        &directory,
        "assert_eq!(fixture::entry(std::hint::black_box(42)), 42 ^ 17);",
    );
}

#[test]
fn scalar_generic_instantiations_have_distinct_object_identities() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let source = "pub mod identity { #[inline(never)] pub fn generic<T>(x: T) -> T { x } } #[inline(never)] pub fn entry(x: u32) -> u32 { identity::generic(x) }";
    let first = stats(&compile(&root.path().join("u32"), &cache, source, &[]));
    assert_eq!(first.hits, 0);
    let warm = stats(&compile(&root.path().join("u32-warm"), &cache, source, &[]));
    assert_eq!(
        warm.hits, warm.cgus,
        "scalar generic instances and callers must be reusable: {warm:?}"
    );
    run(
        &root.path().join("u32-warm"),
        "assert_eq!(fixture::entry(std::hint::black_box(u32::MAX)), u32::MAX);",
    );
    let wide_source = source.replace("u32", "u64");
    let wide = stats(&compile(
        &root.path().join("u64"),
        &cache,
        &wide_source,
        &[],
    ));
    assert_eq!(
        wide.hits, 0,
        "different substituted types must not share objects: {wide:?}"
    );
    let wide_warm = stats(&compile(
        &root.path().join("u64-warm"),
        &cache,
        &wide_source,
        &[],
    ));
    assert_eq!(wide_warm.hits, wide_warm.cgus, "{wide_warm:?}");
    run(
        &root.path().join("u64-warm"),
        "assert_eq!(fixture::entry(std::hint::black_box(u64::MAX)), u64::MAX);",
    );
}

#[test]
fn changed_trait_impl_cannot_restore_stale_generic_code() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let source = "trait Value { fn value(self) -> u64; } impl Value for u64 { #[inline(always)] fn value(self) -> u64 { self ^ 7 } } #[inline(never)] fn generic<T: Value>(x: T) -> u64 { x.value() } #[inline(never)] pub fn entry(x: u64) -> u64 { generic(x) }";
    let first_directory = root.path().join("first");
    stats(&compile(&first_directory, &cache, source, &[]));
    run(
        &first_directory,
        "assert_eq!(fixture::entry(std::hint::black_box(42)), 42 ^ 7);",
    );
    let changed_directory = root.path().join("changed");
    let changed_source = source.replace("self ^ 7", "self ^ 9");
    let changed = stats(&compile(&changed_directory, &cache, &changed_source, &[]));
    assert!(
        changed.misses > 0,
        "changed implementation must require code generation: {changed:?}"
    );
    let before: serde_json::Value =
        serde_json::from_slice(&std::fs::read(first_directory.join("hashes.json")).unwrap())
            .unwrap();
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(changed_directory.join("hashes.json")).unwrap())
            .unwrap();
    assert_eq!(
        before["items"]["generic"]["hash"], after["items"]["generic"]["hash"],
        "regression requires unchanged generic HIR identity"
    );
    assert!(before["items"]["generic"]["hash"].is_string());
    run(
        &changed_directory,
        "assert_eq!(fixture::entry(std::hint::black_box(42)), 42 ^ 9);",
    );
    let warm_directory = root.path().join("warm");
    let warm = stats(&compile(&warm_directory, &cache, &changed_source, &[]));
    assert_eq!(warm.hits, warm.cgus, "{warm:?}");
    run(
        &warm_directory,
        "assert_eq!(fixture::entry(std::hint::black_box(42)), 42 ^ 9);",
    );
}

#[test]
fn concurrent_identical_cross_crate_writers_publish_one_object() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let first_directory = root.path().join("a");
    let second_directory = root.path().join("b");
    let source = "#[inline(never)] pub fn entry(x: u64) -> u64 { x ^ 17 }";
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            compile_named(&first_directory, &cache, source, &[], "crate_a")
        });
        let second = scope.spawn(|| {
            barrier.wait();
            compile_named(&second_directory, &cache, source, &[], "crate_b")
        });
        stats(&first.join().unwrap());
        stats(&second.join().unwrap());
    });
    let assertion = "assert_eq!(fixture::entry(std::hint::black_box(42)), 42 ^ 17);";
    run_named(&first_directory, assertion, "crate_a");
    run_named(&second_directory, assertion, "crate_b");
    let third_directory = root.path().join("c");
    let warm = stats(&compile_named(
        &third_directory,
        &cache,
        source,
        &[],
        "crate_c",
    ));
    assert_eq!(warm.hits, warm.cgus, "{warm:?}");
    run_named(&third_directory, assertion, "crate_c");
}

#[test]
fn direct_call_relocations_are_rebound_across_crates() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let source = "pub mod helper { #[inline(never)] pub fn leaf(x: u64) -> u64 { x ^ 17 } } #[inline(never)] pub fn entry(x: u64) -> u64 { helper::leaf(x) }";
    let first_directory = root.path().join("a");
    let first = stats(&compile_named(
        &first_directory,
        &cache,
        source,
        &[],
        "crate_a",
    ));
    assert_eq!(first.cgus, 2);
    assert_eq!(first.hits, 0);
    let second_directory = root.path().join("b");
    let second = stats(&compile_named(
        &second_directory,
        &cache,
        source,
        &[],
        "crate_b",
    ));
    assert_eq!(second.cgus, 2);
    assert_eq!(second.hits, second.cgus, "{second:?}");
    let assertion = "assert_eq!(fixture::entry(std::hint::black_box(42)), 42 ^ 17);";
    run_named(&first_directory, assertion, "crate_a");
    run_named(&second_directory, assertion, "crate_b");
}

#[test]
fn callee_body_change_invalidates_caller_but_preserves_sibling() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let source = "pub mod helper { #[inline(never)] pub fn leaf(x: u64) -> u64 { x ^ 17 } } pub mod independent { #[inline(never)] pub fn sibling(x: u64) -> u64 { x ^ 31 } } #[inline(never)] pub fn entry(x: u64) -> u64 { helper::leaf(x) }";
    let first_directory = root.path().join("first");
    let flags = ["-Ccodegen-units=3"];
    let first = stats(&compile(&first_directory, &cache, source, &flags));
    assert_eq!(first.cgus, 3);
    assert_eq!(first.hits, 0);
    let changed_directory = root.path().join("changed");
    let changed_source = source.replace("x ^ 17", "x ^ 19");
    let changed = stats(&compile(
        &changed_directory,
        &cache,
        &changed_source,
        &flags,
    ));
    assert_eq!(changed.cgus, 3);
    assert_eq!(
        changed.hits, 1,
        "only the independent sibling may hit: {changed:?}"
    );
    assert_eq!(
        changed.misses, 2,
        "callee and caller must miss: {changed:?}"
    );
    run(
        &first_directory,
        "assert_eq!(fixture::entry(std::hint::black_box(42)), 42 ^ 17); assert_eq!(fixture::independent::sibling(std::hint::black_box(42)), 42 ^ 31);",
    );
    run(
        &changed_directory,
        "assert_eq!(fixture::entry(std::hint::black_box(42)), 42 ^ 19); assert_eq!(fixture::independent::sibling(std::hint::black_box(42)), 42 ^ 31);",
    );
    let warm_directory = root.path().join("warm");
    let warm = stats(&compile(&warm_directory, &cache, &changed_source, &flags));
    assert_eq!(warm.hits, warm.cgus);
    run(
        &warm_directory,
        "assert_eq!(fixture::entry(std::hint::black_box(42)), 42 ^ 19); assert_eq!(fixture::independent::sibling(std::hint::black_box(42)), 42 ^ 31);",
    );
}

#[test]
fn external_aggregate_objects_are_reused_and_execute() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let source = "pub fn entry(value: u64) -> u64 { let mut values = Vec::new(); for i in 0..value { values.push(i); } values.iter().sum::<u64>() + 7 }";
    let flags = ["-Ccodegen-units=16", "-Copt-level=0"];
    stats(&compile(&root.path().join("first"), &cache, source, &flags));
    let directory = root.path().join("changed");
    let changed = stats(&compile(
        &directory,
        &cache,
        &source.replace("+ 7", "+ 9"),
        &flags,
    ));
    assert!(
        changed.hits > 0,
        "external aggregate CGUs must reuse: {changed:?}"
    );
    run(
        &directory,
        "assert_eq!(fixture::entry(std::hint::black_box(10)), 54);",
    );
}

#[test]
fn external_generic_with_local_drop_impl_bypasses() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let source = "static VALUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0); struct Record; impl Drop for Record { fn drop(&mut self) { VALUE.store(7, std::sync::atomic::Ordering::SeqCst); } } pub fn entry() -> u64 { drop(Box::new(Record)); VALUE.load(std::sync::atomic::Ordering::SeqCst) }";
    let flags = ["-Ccodegen-units=16", "-Copt-level=0"];
    stats(&compile(&root.path().join("first"), &cache, source, &flags));
    let directory = root.path().join("changed");
    stats(&compile(
        &directory,
        &cache,
        &source.replace("store(7", "store(9"),
        &flags,
    ));
    run(&directory, "assert_eq!(fixture::entry(), 9);");
}

#[test]
fn external_aggregate_relocations_survive_crate_rename() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let source = "pub fn entry(value: u64) -> u64 { let mut values = Vec::new(); for i in 0..value { values.push(i); } values.iter().sum::<u64>() + 7 }";
    let flags = ["-Ccodegen-units=16", "-Copt-level=0"];
    stats(&compile_named(
        &root.path().join("a"),
        &cache,
        source,
        &flags,
        "crate_a",
    ));
    let directory = root.path().join("b");
    stats(&compile_named(
        &directory, &cache, source, &flags, "crate_b",
    ));
    run_named(
        &directory,
        "assert_eq!(fixture::entry(std::hint::black_box(10)), 52);",
        "crate_b",
    );
}
