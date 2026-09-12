use std::path::Path;
use std::process::{Command, Output};

fn run(directory: &Path, source: &str, extra: &[&str]) -> Output {
    std::fs::write(directory.join("input.rs"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
        .current_dir(directory)
        .args([
            "input.rs",
            "--crate-name",
            "fixture",
            "--crate-type",
            "rlib",
            "--edition=2024",
            "-Awarnings",
        ])
        .args(extra)
        .env("LOOM_ITEM_HASHES", directory.join("hashes.json"))
        .env("LOOM_ITEM_PREIMAGES", directory.join("preimages"))
        .output()
        .unwrap()
}

fn json(directory: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(directory.join("hashes.json")).unwrap()).unwrap()
}

#[test]
fn rustc_version_and_error_exit_are_preserved() {
    let version = Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
        .arg("-vV")
        .output()
        .unwrap();
    let native = Command::new(concat!(env!("HASH_RUSTC_SYSROOT"), "/bin/rustc"))
        .arg("-vV")
        .output()
        .unwrap();
    assert_eq!(version.status.code(), native.status.code());
    assert_eq!(version.stdout, native.stdout);
    let directory = tempfile::tempdir().unwrap();
    assert!(
        run(directory.path(), "pub fn entry() {}", &[])
            .status
            .success()
    );
    let failure = run(directory.path(), "pub fn entry() -> u32 { false }", &[]);
    assert_eq!(failure.status.code(), Some(1));
    assert!(
        !directory.path().join("hashes.json").exists(),
        "failed builds cannot leave stale identity"
    );
}

#[test]
fn plain_public_root_functions_select_entries() {
    let directory = tempfile::tempdir().unwrap();
    let result = run(
        directory.path(),
        "pub fn entry() {} fn private() {} pub mod nested { pub fn visible() {} fn private() {} }",
        &[],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let document = json(directory.path());
    let entries = document["entry"].as_object().unwrap();
    assert_eq!(entries.len(), 1, "{document:#}");
    assert!(
        entries
            .keys()
            .any(|path| path == "entry" || path.ends_with("::entry"))
    );
}

#[test]
fn generics_shadowing_closures_and_macro_expansion_are_alpha_equivalent() {
    let directory = tempfile::tempdir().unwrap();
    let source = "macro_rules! plus { ($a:expr) => { $a + 1 } } fn generic<T>(x: T) -> T { x } pub fn entry(x: u32) -> u32 { let f = |y| plus!(y); let x = generic(f(x)); { let z = x; z } }";
    let first = run(directory.path(), source, &[]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let before = json(directory.path());
    let second = run(
        directory.path(),
        "macro_rules! plus { ($a:expr) => { $a + 1 } } fn generic<Other>(renamed: Other) -> Other { renamed } pub fn entry(renamed: u32) -> u32 { let f = |arg| plus!(arg); let renamed = generic(f(renamed)); { let z = renamed; z } }",
        &[],
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(before["entry"], json(directory.path())["entry"]);
}

#[test]
fn trait_dispatch_keeps_generic_identity_but_tracks_nominal_impls() {
    let directory = tempfile::tempdir().unwrap();
    let source = "trait Value { fn value(&self) -> u32; } struct A; impl Value for A { fn value(&self) -> u32 { 1 } } fn generic<T: Value>(x: T) -> u32 { x.value() } pub fn entry() -> u32 { generic(A) }";
    let first = run(directory.path(), source, &[]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let before = json(directory.path());
    let second = run(directory.path(), &source.replace("{ 1 }", "{ 2 }"), &[]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let after = json(directory.path());
    assert_ne!(before["entry"], after["entry"]);
    assert_ne!(before["items"]["A"]["hash"], after["items"]["A"]["hash"]);
    assert_eq!(
        before["items"]["generic"]["hash"],
        after["items"]["generic"]["hash"]
    );
    assert_ne!(before["items"], after["items"]);
    let generic = before["items"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(name, _)| name.ends_with("generic"))
        .unwrap()
        .1;
    assert!(
        generic["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name.as_str().unwrap().ends_with("Value::value"))
    );
}

#[test]
fn async_spans_and_loop_labels_do_not_enter_hashes() {
    let directory = tempfile::tempdir().unwrap();
    let source = "pub async fn entry(mut n: u32) -> u32 { 'outer: loop { if n == 0 { break 'outer n; } n -= 1; } }";
    let first = run(directory.path(), source, &[]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let before = json(directory.path());
    let second = run(
        directory.path(),
        &source.replace("'outer", "'renamed").replace("{", "{\n  "),
        &[],
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(before["entry"], json(directory.path())["entry"]);
}

#[test]
fn representation_and_track_caller_change_identity() {
    let directory = tempfile::tempdir().unwrap();
    let source = "struct A { a: u8, b: u32 } pub fn entry(value: A) -> u32 { value.b }";
    assert!(run(directory.path(), source, &[]).status.success());
    let before = json(directory.path());
    assert!(
        run(directory.path(), &format!("#[repr(packed)] {source}"), &[])
            .status
            .success()
    );
    assert_ne!(before["entry"], json(directory.path())["entry"]);
    let source = "pub fn entry() -> u32 { 1 }";
    assert!(run(directory.path(), source, &[]).status.success());
    let before = json(directory.path());
    assert!(
        run(directory.path(), &format!("#[track_caller] {source}"), &[])
            .status
            .success()
    );
    assert_ne!(before["entry"], json(directory.path())["entry"]);
}

#[test]
fn native_executable_still_runs() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("main.rs");
    let executable = directory.path().join("native-smoke");
    std::fs::write(
        &input,
        "pub fn entry() -> i32 { 23 } fn main() { std::process::exit(entry()); }",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
        .arg(input)
        .arg("-o")
        .arg(&executable)
        .env("LOOM_ITEM_HASHES", directory.path().join("hashes.json"))
        .env("LOOM_ITEM_PREIMAGES", directory.path().join("preimages"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(Command::new(executable).status().unwrap().code(), Some(23));
    assert_eq!(
        json(directory.path())["entry"].as_object().unwrap().len(),
        1
    );
}

#[test]
fn external_metadata_identity_moves_entry() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("dependency.rs"),
        "pub fn value() -> u32 { 1 }",
    )
    .unwrap();
    let build_dependency = |metadata: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
            .current_dir(directory.path())
            .args([
                "dependency.rs",
                "--crate-name=dependency",
                "--crate-type=rlib",
                "-C",
                metadata,
            ])
            .env_remove("LOOM_ITEM_HASHES")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    build_dependency("metadata=first");
    let source = "pub fn entry() -> u32 { dependency::value() }";
    assert!(
        run(
            directory.path(),
            source,
            &["--extern=dependency=libdependency.rlib"]
        )
        .status
        .success()
    );
    let before = json(directory.path());
    build_dependency("metadata=second");
    assert!(
        run(
            directory.path(),
            source,
            &["--extern=dependency=libdependency.rlib"]
        )
        .status
        .success()
    );
    assert_ne!(before["entry"], json(directory.path())["entry"]);
}

#[test]
fn assembly_template_is_hashed_without_spans() {
    let directory = tempfile::tempdir().unwrap();
    let source =
        "pub fn entry() { unsafe { core::arch::asm!(\"nop\", options(nomem, nostack)); } }";
    assert!(run(directory.path(), source, &[]).status.success());
    let before = json(directory.path());
    assert!(
        run(
            directory.path(),
            &source.replace("unsafe", "\n unsafe"),
            &[]
        )
        .status
        .success()
    );
    assert_eq!(before["entry"], json(directory.path())["entry"]);
    assert!(
        run(directory.path(), &source.replace("nop", "nop; nop"), &[])
            .status
            .success()
    );
    assert_ne!(before["entry"], json(directory.path())["entry"]);
}

#[test]
fn associated_signature_path_produces_identity() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(
        directory.path(),
        "trait Value { type Output; } pub fn entry<T: Value>(x: T::Output) -> T::Output { x }",
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(directory.path().join("hashes.json").exists());
}

#[test]
fn external_crate_content_changes_identity_with_fixed_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let dependency = directory.path().join("dependency.rs");
    let build_dependency = |literal: u32| {
        std::fs::write(
            &dependency,
            format!("pub fn value() -> u32 {{ {literal} }}"),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
            .current_dir(directory.path())
            .args([
                "dependency.rs",
                "--crate-type=rlib",
                "--crate-name=dependency",
                "-Cmetadata=fixed",
            ])
            .env_remove("LOOM_OBJECT_CACHE")
            .env_remove("LOOM_ITEM_HASHES")
            .env_remove("LOOM_ITEM_COVERAGE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let source = "pub fn entry() -> u32 { dependency::value() }";
    build_dependency(7);
    assert!(
        run(
            directory.path(),
            source,
            &["--extern=dependency=libdependency.rlib"]
        )
        .status
        .success()
    );
    let before = json(directory.path());
    build_dependency(9);
    assert!(
        run(
            directory.path(),
            source,
            &["--extern=dependency=libdependency.rlib"]
        )
        .status
        .success()
    );
    assert_ne!(before["entry"], json(directory.path())["entry"]);
}
