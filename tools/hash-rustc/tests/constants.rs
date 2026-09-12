use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};

fn run(directory: &Path, source: &str) -> Output {
    std::fs::write(directory.join("input.rs"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
        .current_dir(directory)
        .args([
            "input.rs",
            "--crate-name=fixture",
            "--crate-type=rlib",
            "--edition=2024",
            "-Awarnings",
        ])
        .env("LOOM_ITEM_HASHES", directory.join("hashes.json"))
        .env("LOOM_ITEM_PREIMAGES", directory.join("preimages"))
        .env_remove("LOOM_HANDLER_ROWS")
        .output()
        .unwrap()
}

fn compile(source: &str) -> Value {
    let directory = tempfile::tempdir().unwrap();
    let output = run(directory.path(), source);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&std::fs::read(directory.path().join("hashes.json")).unwrap()).unwrap()
}

#[test]
fn schema_is_evaluated_by_rustc() {
    let document = compile(
        r#"
const CONTENT: &str = "{\"type\":\"object\"}";
const fn schema() -> &'static str { CONTENT }
pub const LOOM_SCHEMA: &str = schema();
pub fn entry() {}
"#,
    );
    assert_eq!(document["schema"], json!("{\"type\":\"object\"}"));
}

#[test]
fn absent_schema_is_null_and_plain_functions_infer_rows() {
    let document = compile("fn helper() {} pub fn entry() { helper(); }");
    assert_eq!(document["schema"], Value::Null);
    let entries = document["effects"]["entries"].as_object().unwrap();
    assert_eq!(entries.len(), 1);
    let row = entries.values().next().unwrap();
    assert_eq!(row["labels"], json!([]));
    assert_eq!(row["unknown"], json!([]));
}

#[test]
fn malformed_schema_constant_type_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(
        directory.path(),
        "pub const LOOM_SCHEMA: u32 = 7; pub fn entry() {}",
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("LOOM_SCHEMA"));
}

#[test]
fn exported_schema_is_part_of_entry_identity() {
    let empty = compile("pub fn entry() {}");
    let first = compile("pub const LOOM_SCHEMA: &str = \"CREATE TABLE a(id);\"; pub fn entry() {}");
    let second =
        compile("pub const LOOM_SCHEMA: &str = \"CREATE TABLE b(id);\"; pub fn entry() {}");
    assert_ne!(empty["entry"]["entry"], first["entry"]["entry"]);
    assert_ne!(first["entry"]["entry"], second["entry"]["entry"]);
    let unrelated = compile("pub const UNRELATED: u32 = 1; pub fn entry() {}");
    assert_eq!(empty["entry"]["entry"], unrelated["entry"]["entry"]);
}
