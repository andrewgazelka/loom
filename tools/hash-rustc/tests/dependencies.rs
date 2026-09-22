//! Content-linked references into Loom definition dependencies (`LOOM_DEP_ITEMS`).
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

fn driver(directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hash-rustc"));
    command
        .current_dir(directory)
        .env_remove("LOOM_OBJECT_CACHE")
        .env_remove("LOOM_ITEM_COVERAGE");
    command
}

fn successful(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// Compile the dependency as an rlib with the given metadata and record its
/// item document, exactly as the dependency's own definition build does.
fn build_dependency(directory: &Path, source: &str, metadata: &str) -> Value {
    std::fs::write(directory.join("dependency.rs"), source).unwrap();
    let document = directory.join("dependency-hashes.json");
    successful(
        driver(directory)
            .args([
                "dependency.rs",
                "--crate-name=dependency",
                "--crate-type=rlib",
                "--edition=2024",
                "-Awarnings",
                "-C",
                metadata,
            ])
            .env("LOOM_ITEM_HASHES", &document)
            .env(
                "LOOM_ITEM_PREIMAGES",
                directory.join("dependency-preimages"),
            )
            .output()
            .unwrap(),
    );
    read_json(&document)
}

/// Stage `document` as the stored items of crate `dependency` under
/// `LOOM_DEP_ITEMS`. Staging is by crate name: `<directory>/dependency.json`.
fn stage(directory: &Path, document: &Value) {
    let items = directory.join("dep-items");
    std::fs::create_dir_all(&items).unwrap();
    std::fs::write(
        items.join("dependency.json"),
        serde_json::to_vec(document).unwrap(),
    )
    .unwrap();
}

fn run_dependent(directory: &Path, source: &str) -> Output {
    std::fs::write(directory.join("input.rs"), source).unwrap();
    let items = directory.join("dep-items");
    std::fs::create_dir_all(&items).unwrap();
    driver(directory)
        .args([
            "input.rs",
            "--crate-name=fixture",
            "--crate-type=rlib",
            "--edition=2024",
            "-Awarnings",
            "--extern=dependency=libdependency.rlib",
        ])
        .env("LOOM_ITEM_HASHES", directory.join("hashes.json"))
        .env("LOOM_ITEM_PREIMAGES", directory.join("preimages"))
        .env("LOOM_DEP_ITEMS", &items)
        .output()
        .unwrap()
}

fn compile_dependent(directory: &Path, source: &str) -> Value {
    successful(run_dependent(directory, source));
    read_json(&directory.join("hashes.json"))
}

const DEPENDENT: &str = "pub fn entry() -> u32 { dependency::value() }";

#[test]
fn referenced_item_change_moves_the_dependent() {
    let directory = tempfile::tempdir().unwrap();
    let dependency = "pub fn value() -> u32 { 7 } pub fn helper() -> u32 { 1 }";
    stage(
        directory.path(),
        &build_dependency(directory.path(), dependency, "metadata=fixed"),
    );
    let before = compile_dependent(directory.path(), DEPENDENT);
    assert!(
        before["items"]["entry"]["refs"]
            .as_array()
            .unwrap()
            .contains(&Value::from("dependency::value")),
        "{before:#}"
    );
    stage(
        directory.path(),
        &build_dependency(
            directory.path(),
            &dependency.replace("{ 7 }", "{ 9 }"),
            "metadata=fixed",
        ),
    );
    let after = compile_dependent(directory.path(), DEPENDENT);
    assert_ne!(before["entry"], after["entry"]);
    assert_ne!(before["exports"], after["exports"]);
}

/// The Unison property: the caller's identity is a function of the callee's
/// content, not of the dependency crate's strict version hash. An unreferenced
/// helper edit and a different `-C metadata` both move the SVH; neither moves
/// the dependent.
#[test]
fn unreferenced_helper_and_crate_metadata_leave_the_dependent_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let dependency = "pub fn value() -> u32 { 7 } pub fn helper() -> u32 { 1 }";
    let first = build_dependency(directory.path(), dependency, "metadata=first");
    stage(directory.path(), &first);
    let before = compile_dependent(directory.path(), DEPENDENT);
    let second = build_dependency(
        directory.path(),
        &dependency.replace("{ 1 }", "{ 2 }"),
        "metadata=second",
    );
    assert_ne!(first["items"]["helper"], second["items"]["helper"]);
    assert_eq!(first["items"]["value"], second["items"]["value"]);
    stage(directory.path(), &second);
    let after = compile_dependent(directory.path(), DEPENDENT);
    assert_eq!(before["entry"], after["entry"]);
    assert_eq!(before["items"], after["items"]);
}

#[test]
fn local_renaming_inside_the_dependency_leaves_the_dependent_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let dependency = "pub fn value() -> u32 { let total = 7; total }";
    stage(
        directory.path(),
        &build_dependency(directory.path(), dependency, "metadata=fixed"),
    );
    let before = compile_dependent(directory.path(), DEPENDENT);
    stage(
        directory.path(),
        &build_dependency(
            directory.path(),
            &dependency.replace("total", "sum"),
            "metadata=fixed",
        ),
    );
    let after = compile_dependent(directory.path(), DEPENDENT);
    assert_eq!(before["entry"], after["entry"]);
}

/// Without a staged document the crate is not a Loom dependency and the
/// whole-crate rule applies: crate metadata alone moves the dependent.
#[test]
fn unmapped_crate_keeps_the_whole_crate_rule() {
    let directory = tempfile::tempdir().unwrap();
    let dependency = "pub fn value() -> u32 { 7 }";
    build_dependency(directory.path(), dependency, "metadata=first");
    let before = compile_dependent(directory.path(), DEPENDENT);
    build_dependency(directory.path(), dependency, "metadata=second");
    let after = compile_dependent(directory.path(), DEPENDENT);
    assert_ne!(before["entry"], after["entry"]);
}

/// A staged document that never recorded the referent is a hard error naming
/// the crate and the path; the SVH is not substituted.
#[test]
fn referent_missing_from_the_staged_document_is_fatal() {
    let directory = tempfile::tempdir().unwrap();
    let without_value = build_dependency(
        directory.path(),
        "pub fn other() -> u32 { 1 }",
        "metadata=fixed",
    );
    build_dependency(
        directory.path(),
        "pub fn value() -> u32 { 7 } pub fn other() -> u32 { 1 }",
        "metadata=fixed",
    );
    stage(directory.path(), &without_value);
    let output = run_dependent(directory.path(), DEPENDENT);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("dependency crate dependency has no stored item value"),
        "{stderr}"
    );
    assert!(stderr.contains("dependency.json"), "{stderr}");
    assert!(!directory.path().join("hashes.json").exists());
}

#[test]
fn malformed_staging_entries_are_rejected_by_name() {
    let directory = tempfile::tempdir().unwrap();
    build_dependency(
        directory.path(),
        "pub fn value() -> u32 { 7 }",
        "metadata=fixed",
    );
    let items = directory.path().join("dep-items");
    std::fs::create_dir_all(&items).unwrap();
    std::fs::write(items.join("notes.txt"), b"not a document").unwrap();
    let output = run_dependent(directory.path(), DEPENDENT);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("notes.txt"), "{stderr}");
    assert!(stderr.contains("<crate_name>.json"), "{stderr}");
}

/// Variants and constructors of a dependency type are not items of their own:
/// the reference resolves to the enclosing type's stored hash, so adding a
/// variant moves the dependent while the reference list names the type.
#[test]
fn variants_and_constructors_resolve_to_the_enclosing_dependency_type() {
    let directory = tempfile::tempdir().unwrap();
    let dependency = "pub enum Shape { Circle(f64), Square(f64) } pub struct Point(pub u32); pub fn value() -> u32 { 7 }";
    let dependent = "pub fn entry() -> f64 { match dependency::Shape::Circle(1.0) { dependency::Shape::Circle(r) => r, _ => 0.0 } } pub fn point() -> u32 { dependency::Point(3).0 }";
    stage(
        directory.path(),
        &build_dependency(directory.path(), dependency, "metadata=fixed"),
    );
    let before = compile_dependent(directory.path(), dependent);
    let refs = |document: &Value, item: &str| -> Vec<String> {
        serde_json::from_value(document["items"][item]["refs"].clone()).unwrap()
    };
    assert!(refs(&before, "entry").contains(&"dependency::Shape".to_owned()));
    assert!(
        refs(&before, "entry")
            .iter()
            .all(|reference| !reference.contains("Circle")),
        "{before:#}"
    );
    assert!(refs(&before, "point").contains(&"dependency::Point".to_owned()));
    stage(
        directory.path(),
        &build_dependency(
            directory.path(),
            &dependency.replace("Square(f64)", "Square(f64), Triangle(f64)"),
            "metadata=fixed",
        ),
    );
    let after = compile_dependent(directory.path(), dependent);
    assert_ne!(before["exports"]["entry"], after["exports"]["entry"]);
    assert_eq!(before["exports"]["point"], after["exports"]["point"]);
}
