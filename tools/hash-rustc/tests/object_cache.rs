use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug)]
struct Stats {
    cgus: u64,
    hits: u64,
    misses: u64,
    bytes_reused: u64,
}

fn stats(output: &Output) -> Stats {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let lines: Vec<_> = stderr
        .lines()
        .filter(|line| line.starts_with("object-cache: "))
        .collect();
    assert_eq!(lines.len(), 1, "{stderr}");
    let values: std::collections::BTreeMap<_, _> = lines[0]
        .split_whitespace()
        .skip(1)
        .map(|field| field.split_once('=').unwrap())
        .collect();
    assert_eq!(values.len(), 4, "{stderr}");
    let result = Stats {
        cgus: values["cgus"].parse().unwrap(),
        hits: values["hits"].parse().unwrap(),
        misses: values["misses"].parse().unwrap(),
        bytes_reused: values["bytes_reused"].parse().unwrap(),
    };
    assert_eq!(result.hits + result.misses, result.cgus, "{result:?}");
    result
}

fn source(changed: bool) -> String {
    let constant = if changed { 9 } else { 7 };
    format!(
        "pub mod stable {{ #[inline(never)] pub fn value(x: u64) -> u64 {{ x ^ 31 }} }}\npub mod changed {{ #[inline(never)] pub fn value(x: u64) -> u64 {{ x ^ {constant} }} }}"
    )
}

fn compile(
    directory: &Path,
    cache: &Path,
    name: &str,
    changed: bool,
    opt: &str,
    fail: bool,
) -> Output {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(directory.join("fixture.rs"), source(changed)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hash-rustc"));
    command
        .current_dir(directory)
        .args([
            "fixture.rs",
            "--crate-type=rlib",
            "--edition=2024",
            "--crate-name",
            name,
            "-C",
            "lto=off",
            "-C",
            "embed-bitcode=no",
            "-C",
            "debuginfo=0",
            "-C",
            "codegen-units=2",
            "-C",
            &format!("opt-level={opt}"),
        ])
        .env_remove("LOOM_ITEM_HASHES")
        .env_remove("LOOM_ITEM_PREIMAGES")
        .env("LOOM_OBJECT_CACHE", cache)
        .env("LOOM_OBJECT_CACHE_STATS", "1")
        .env_remove("LOOM_OBJECT_CACHE_FAIL_PUBLISH");
    if fail {
        command.env("LOOM_OBJECT_CACHE_FAIL_PUBLISH", "1");
    }
    command.output().unwrap()
}

fn run(directory: &Path, name: &str, changed: bool) {
    let expected = if changed { 9 } else { 7 };
    std::fs::write(directory.join("main.rs"), format!(
        "fn main() {{ assert_eq!({name}::stable::value(std::hint::black_box(42)), 42 ^ 31); assert_eq!({name}::changed::value(std::hint::black_box(42)), 42 ^ {expected}); }}"
    )).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
        .current_dir(directory)
        .args([
            "main.rs",
            "--edition=2024",
            "--extern",
            &format!("{name}=lib{name}.rlib"),
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

#[test]
fn second_build_of_same_crate_hits_all_cgus() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let cold = stats(&compile(
        &root.path().join("first"),
        &cache,
        "fixture",
        false,
        "2",
        false,
    ));
    assert_eq!(
        cold.cgus, 2,
        "fixture must isolate the two functions: {cold:?}"
    );
    assert_eq!(cold.hits, 0);
    let warm = stats(&compile(
        &root.path().join("second"),
        &cache,
        "fixture",
        false,
        "2",
        false,
    ));
    assert_eq!(warm.cgus, cold.cgus);
    assert_eq!(warm.hits, warm.cgus);
    assert!(warm.bytes_reused > 0);
    run(&root.path().join("first"), "fixture", false);
    run(&root.path().join("second"), "fixture", false);
}

#[test]
fn identical_function_in_two_crates_shares_object() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let first = stats(&compile(
        &root.path().join("a"),
        &cache,
        "crate_a",
        false,
        "2",
        false,
    ));
    assert_eq!(first.hits, 0);
    let second = stats(&compile(
        &root.path().join("b"),
        &cache,
        "crate_b",
        true,
        "2",
        false,
    ));
    assert_eq!(second.cgus, 2);
    assert_eq!(second.hits, 1, "{second:?}");
    assert_eq!(second.misses, 1);
    run(&root.path().join("a"), "crate_a", false);
    run(&root.path().join("b"), "crate_b", true);
}

#[test]
fn changed_body_misses() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    stats(&compile(
        &root.path().join("first"),
        &cache,
        "fixture",
        false,
        "2",
        false,
    ));
    let changed = stats(&compile(
        &root.path().join("changed"),
        &cache,
        "fixture",
        true,
        "2",
        false,
    ));
    assert_eq!(changed.cgus, 2);
    assert_eq!(changed.hits, 1);
    assert_eq!(changed.misses, 1);
    run(&root.path().join("changed"), "fixture", true);
}

#[test]
fn flag_change_misses() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    stats(&compile(
        &root.path().join("first"),
        &cache,
        "fixture",
        false,
        "2",
        false,
    ));
    let changed = stats(&compile(
        &root.path().join("changed"),
        &cache,
        "fixture",
        false,
        "3",
        false,
    ));
    assert_eq!(changed.cgus, 2);
    assert_eq!(changed.hits, 0);
    assert_eq!(changed.misses, changed.cgus);
    run(&root.path().join("changed"), "fixture", false);
}

fn objects(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if !directory.exists() {
        return found;
    }
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(objects(&path));
        } else if path.extension().is_some_and(|extension| extension == "o") {
            found.push(path);
        }
    }
    found
}

#[test]
fn partial_write_never_visible() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let failed = compile(
        &root.path().join("failed"),
        &cache,
        "fixture",
        false,
        "2",
        true,
    );
    assert!(
        !failed.status.success(),
        "publish failure injection must fail compilation"
    );
    assert!(
        objects(&cache).is_empty(),
        "failed publish must not expose final objects"
    );
    let retry = stats(&compile(
        &root.path().join("retry"),
        &cache,
        "fixture",
        false,
        "2",
        false,
    ));
    assert_eq!(retry.cgus, 2);
    assert_eq!(retry.hits, 0);
    assert_eq!(retry.misses, retry.cgus);
    for path in objects(&cache) {
        assert!(std::fs::metadata(path).unwrap().len() > 0);
    }
    run(&root.path().join("retry"), "fixture", false);
    let warm = stats(&compile(
        &root.path().join("warm"),
        &cache,
        "fixture",
        false,
        "2",
        false,
    ));
    assert_eq!(warm.hits, warm.cgus);
    run(&root.path().join("warm"), "fixture", false);
}
