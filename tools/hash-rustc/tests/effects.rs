use serde_json::{Value, json};

#[path = "support/effects_fixture.rs"]
mod effects_fixture;
use effects_fixture::{assert_rejected, compile};

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
fn non_literal_perform_is_rejected_with_span() {
    assert_rejected(
        "pub fn entry(label: &str) {\n    renamed::perform(label, ());\n}\n",
        "input.rs:2:5",
    );
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
    assert_rejected(
        "pub fn entry() {\n    let label = \"sleep\";\n    renamed::perform(label, ());\n}\n",
        "input.rs:3:5",
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
    assert_rejected(
        "pub fn entry() { renamed::external_unknown(\"sleep\"); }",
        "sdk.rs:",
    );
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

#[test]
fn arbitrary_sdk_wrapper_is_inferred_from_its_body() {
    assert_known_labels(
        "pub fn entry() { renamed::arbitrary_wrapper(); }",
        json!(["custom.arbitrary"]),
    );
}
