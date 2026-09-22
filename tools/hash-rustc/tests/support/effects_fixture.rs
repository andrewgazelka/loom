use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const SDK: &str = r#"
pub fn sleep() { perform("sleep", ()); }
pub fn exec() { perform("exec", ()); }
pub fn now() { perform("now", ()); }
pub fn arbitrary_wrapper() { perform("custom.arbitrary", ()); }
pub fn perform(_label: &str, _payload: ()) {}
pub mod isolated {
    pub struct Def<F>(pub &'static str, pub core::marker::PhantomData<F>);
    pub fn call<F, A>(_def: Def<F>, _args: A) -> Result<(), ()> { Ok(()) }
}
pub mod handlers {
    pub fn handle<H: Fn(), B: Fn()>(_labels: &[&str], handler: H, body: B) {
        handler(); body();
    }
    pub fn handle_any<H: Fn(), B: Fn()>(handler: H, body: B) { handler(); body(); }
    pub fn handle_pinned<H: Fn(), B: Fn()>(_hash: &str, handler: H, body: B) {
        handler(); body();
    }
}
pub use handlers::{handle, handle_any, handle_pinned};
pub fn external_total() { handle(&["sleep"], || now(), || sleep()); }
pub fn external_unknown(label: &str) { perform(label, ()); }
pub fn external_erased_callback() {
    let callback: fn() = || sleep();
    invoke_callback(callback);
}
fn invoke_callback(callback: fn()) { callback(); }
"#;

fn successful(output: Output) {
    assert!(
        output.status.success(),
        "compiler failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn run(directory: &Path, source: &str, handler_rows: Value) -> Output {
    std::fs::write(directory.join("sdk.rs"), SDK).unwrap();
    successful(
        Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
            .current_dir(directory)
            .args([
                "sdk.rs",
                "--crate-name=loom_guest_rs",
                "--crate-type=rlib",
                "--edition=2024",
            ])
            .env_remove("LOOM_ITEM_HASHES")
            .env_remove("LOOM_HANDLER_ROWS")
            .output()
            .unwrap(),
    );
    std::fs::write(directory.join("input.rs"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
        .current_dir(directory)
        .args([
            "input.rs",
            "--crate-name=fixture",
            "--crate-type=rlib",
            "--edition=2024",
            "--extern=renamed=libloom_guest_rs.rlib",
            "-Awarnings",
        ])
        .env("LOOM_ITEM_HASHES", directory.join("hashes.json"))
        .env("LOOM_ITEM_PREIMAGES", directory.join("preimages"))
        .env("LOOM_HANDLER_ROWS", handler_rows.to_string())
        .output()
        .unwrap()
}

pub fn compile(directory: &Path, source: &str, handler_rows: Value) -> Value {
    successful(run(directory, source, handler_rows));
    serde_json::from_slice(&std::fs::read(directory.join("hashes.json")).unwrap()).unwrap()
}

pub fn assert_rejected(source: &str, location: &str) {
    let directory = tempfile::tempdir().unwrap();
    let output = run(directory.path(), source, serde_json::json!({}));
    assert!(
        !output.status.success(),
        "dynamic label unexpectedly compiled"
    );
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostics.contains(location), "{diagnostics}");
    assert!(diagnostics.contains("effect label at "), "{diagnostics}");
    assert!(
        diagnostics
            .contains("is not a literal or const; rows are inferred and need a static label"),
        "{diagnostics}"
    );
    assert!(!directory.path().join("hashes.json").exists());
}
