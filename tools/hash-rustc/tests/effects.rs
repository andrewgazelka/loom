use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};

const SDK: &str = r#"
pub fn sleep() {}
pub fn exec() {}
pub fn now() {}
pub fn perform(_label: &str, _payload: ()) {}
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

fn compile(directory: &Path, source: &str, handler_rows: Value) -> Value {
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
    successful(
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
            .unwrap(),
    );
    serde_json::from_slice(&std::fs::read(directory.join("hashes.json")).unwrap()).unwrap()
}

fn entry_row(document: &Value) -> &Value {
    document["effects"]["entries"]
        .as_object()
        .unwrap_or_else(|| panic!("missing entry effects: {document:#}"))
        .iter()
        .find(|(item, _)| item.as_str() == "entry" || item.ends_with("::entry"))
        .unwrap_or_else(|| panic!("missing entry: {document:#}"))
        .1
}

#[test]
fn sdk_effects_inferred_through_trait_call() {
    let directory = tempfile::tempdir().unwrap();
    let document = compile(
        directory.path(),
        r#"
trait Work { fn run(); }
struct Used;
struct Unused;
impl Work for Used { fn run() { renamed::sleep(); } }
impl Work for Unused { fn run() { renamed::exec(); } }
fn dispatch<T: Work>() { T::run(); }
pub fn entry() { dispatch::<Used>(); }
"#,
        json!({}),
    );
    let row = entry_row(&document);
    assert_eq!(row["labels"], json!(["sleep"]), "{document:#}");
    assert_eq!(row["unknown"], json!([]), "{document:#}");
    let instances = document["effects"]["instances"].as_object().unwrap();
    assert!(
        instances
            .values()
            .any(|row| row["labels"] == json!(["sleep"])),
        "concrete instances must report their effects: {document:#}"
    );
}

#[test]
fn total_handler_removes_label() {
    let directory = tempfile::tempdir().unwrap();
    let document = compile(
        directory.path(),
        r#"
pub fn entry() {
    renamed::handle(
        &["sleep"],
        || renamed::now(),
        || { renamed::sleep(); renamed::perform("fs.read", ()); },
    );
}
"#,
        json!({}),
    );
    let row = entry_row(&document);
    assert_eq!(row["labels"], json!(["fs.read", "now"]), "{document:#}");
    assert_eq!(row["unknown"], json!([]), "{document:#}");
}

#[test]
fn non_literal_perform_is_unknown_with_span() {
    let directory = tempfile::tempdir().unwrap();
    let document = compile(
        directory.path(),
        "pub fn entry(label: &str) {\n    renamed::perform(label, ());\n}\n",
        json!({}),
    );
    let row = entry_row(&document);
    let unknown = row["unknown"].as_array().unwrap();
    assert_eq!(unknown.len(), 1, "{document:#}");
    let item = unknown[0]["item"].as_str().unwrap();
    assert!(item == "entry" || item.ends_with("::entry"), "{document:#}");
    let span = unknown[0]["span"].as_str().unwrap();
    assert!(span.ends_with("input.rs:2:5"), "{document:#}");
}

#[test]
fn handle_with_uses_pinned_row() {
    let directory = tempfile::tempdir().unwrap();
    let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let source = format!(
        r#"
pub fn entry() {{
    renamed::handle_pinned("{hash}", || renamed::now(), || {{
        renamed::sleep();
        renamed::perform("fs.read", ());
    }});
}}
"#
    );
    let document = compile(
        directory.path(),
        &source,
        json!({(hash): {"labels": ["now"], "handled": ["sleep"], "unknown": []}}),
    );
    let row = entry_row(&document);
    assert_eq!(row["labels"], json!(["fs.read", "now"]), "{document:#}");
    assert_eq!(row["unknown"], json!([]), "{document:#}");
}

fn assert_known_labels(source: &str, expected: Value) {
    let directory = tempfile::tempdir().unwrap();
    let document = compile(directory.path(), source, json!({}));
    let row = entry_row(&document);
    assert_eq!(row["labels"], expected, "{document:#}");
    assert_eq!(row["unknown"], json!([]), "{document:#}");
}

#[test]
fn recursive_calls_reach_effect_fixed_point() {
    assert_known_labels(
        r#"
fn first(n: usize) {
    if n == 0 { renamed::sleep(); } else { second(n - 1); }
}
fn second(n: usize) {
    if n == 0 { renamed::now(); } else { first(n - 1); }
}
pub fn entry(n: usize) { first(n); }
"#,
        json!(["now", "sleep"]),
    );
}

#[test]
fn standard_iterator_resolves_effectful_generic_callback() {
    assert_known_labels(
        r#"
fn invoke<F: FnMut(u8)>(callback: F) { [1u8, 2].into_iter().for_each(callback); }
pub fn entry() { invoke(|_| renamed::sleep()); }
"#,
        json!(["sleep"]),
    );
}

#[test]
fn implicit_drop_includes_destructor_effects() {
    assert_known_labels(
        r#"
struct Guard;
impl Drop for Guard { fn drop(&mut self) { renamed::sleep(); } }
pub fn entry() { let _guard = Guard; }
"#,
        json!(["sleep"]),
    );
}

#[test]
fn overloaded_operator_includes_concrete_trait_effects() {
    assert_known_labels(
        r#"
struct Operand;
impl core::ops::Add for Operand {
    type Output = ();
    fn add(self, _other: Self) { renamed::exec(); }
}
pub fn entry() { Operand + Operand; }
"#,
        json!(["exec"]),
    );
}

#[test]
fn nonliteral_binding_dispatch_names_call_site() {
    let directory = tempfile::tempdir().unwrap();
    let document = compile(
        directory.path(),
        "pub fn entry() {\n    let label = \"sleep\";\n    renamed::perform(label, ());\n}\n",
        json!({}),
    );
    let unknown = entry_row(&document)["unknown"].as_array().unwrap();
    assert_eq!(unknown.len(), 1, "{document:#}");
    assert!(
        unknown[0]["span"]
            .as_str()
            .unwrap()
            .ends_with("input.rs:3:5"),
        "diagnostics must name the nonliteral perform call: {document:#}"
    );
}

#[test]
fn nested_generic_instantiations_keep_effect_rows_separate() {
    assert_known_labels(
        r#"
trait Work { fn run(); }
struct Used;
struct Unused;
impl Work for Used { fn run() { renamed::sleep(); } }
impl Work for Unused { fn run() { renamed::exec(); } }
fn inner<T: Work>() { T::run(); }
fn outer<T: Work>() { inner::<T>(); }
pub fn entry() { outer::<Used>(); }
pub fn other_entry() { outer::<Unused>(); }
"#,
        json!(["sleep"]),
    );
}

#[test]
fn user_lookalike_cannot_forge_sdk_effect() {
    assert_known_labels(
        r#"
mod loom_guest_rs { pub fn sleep() {} }
pub fn entry() { loom_guest_rs::sleep(); }
"#,
        json!([]),
    );
}

#[test]
fn local_function_pointer_cast_resolves_effectful_target() {
    assert_known_labels(
        r#"
pub fn entry() {
    let callback = renamed::sleep as fn();
    callback();
}
"#,
        json!(["sleep"]),
    );
}

#[test]
fn external_total_handler_removes_body_effect() {
    assert_known_labels(
        "pub fn entry() { renamed::external_total(); }",
        json!(["now"]),
    );
}

#[test]
fn external_non_literal_dispatch_preserves_sdk_call_site() {
    let directory = tempfile::tempdir().unwrap();
    let document = compile(
        directory.path(),
        "pub fn entry() { renamed::external_unknown(\"sleep\"); }",
        json!({}),
    );
    let unknown = entry_row(&document)["unknown"].as_array().unwrap();
    assert_eq!(unknown.len(), 1, "{document:#}");
    assert!(
        unknown[0]["item"]
            .as_str()
            .unwrap()
            .ends_with("external_unknown")
    );
    assert!(unknown[0]["span"].as_str().unwrap().contains("sdk.rs:"));
}

#[test]
fn literal_perform_through_function_pointer_is_known() {
    assert_known_labels(
        r#"pub fn entry() {
            let perform = renamed::perform as fn(&str, ());
            perform("sleep", ());
        }"#,
        json!(["sleep"]),
    );
}

#[test]
fn erased_function_pointer_callback_keeps_effects() {
    assert_known_labels(
        r#"
fn invoke(callback: fn()) { callback(); }
pub fn entry() { invoke(renamed::sleep as fn()); }
"#,
        json!(["sleep"]),
    );
}

#[test]
fn closure_as_function_pointer_keeps_effects() {
    assert_known_labels(
        r#"
fn invoke(callback: fn()) { callback(); }
pub fn entry() { invoke((|| renamed::sleep()) as fn()); }
"#,
        json!(["sleep"]),
    );
}

#[test]
fn external_erased_callback_keeps_effects() {
    assert_known_labels(
        "pub fn entry() { renamed::external_erased_callback(); }",
        json!(["sleep"]),
    );
}

#[test]
fn constant_perform_labels_are_evaluated() {
    for source in [
        r#"pub fn entry() { const LABEL: &str = "sleep"; renamed::perform(LABEL, ()); }"#,
        r#"const fn label() -> &'static str { "sleep" }
           const LABEL: &str = label();
           pub fn entry() { renamed::perform(LABEL, ()); }"#,
        r#"trait Label { const NAME: &'static str; }
           struct Sleep; impl Label for Sleep { const NAME: &'static str = "sleep"; }
           struct Exec; impl Label for Exec { const NAME: &'static str = "exec"; }
           fn dispatch<T: Label>() { renamed::perform(T::NAME, ()); }
           pub fn entry() { dispatch::<Sleep>(); }"#,
    ] {
        assert_known_labels(source, json!(["sleep"]));
    }
}
