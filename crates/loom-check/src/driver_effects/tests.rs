use super::*;
use crate::Checker;
use loom_proto::{DefineRequest, Lang};

async fn check(source: &str) -> CheckedDef {
    Checker::new()
        .check(&DefineRequest {
            lang: Lang::Rust,
            name: "test".into(),
            source: source.into(),
            deps: BTreeMap::new(),
            allowed_effects: None,
        })
        .await
        .unwrap()
}
fn output(labels: &[&str], unknown: serde_json::Value) -> String {
    serde_json::json!({"existing_driver_key": true, "effects": {
        "entries": {"main": {"labels": labels, "unknown": unknown}}, "instances": {}
    }})
    .to_string()
}

#[tokio::test]
async fn inferred_trait_dispatch() {
    let mut checked = check("trait Action { fn run(); } struct Used; impl Action for Used { fn run() { loom::sleep(1); } } struct Other; impl Action for Other { fn run() { loom::exec(\"other\"); } } fn invoke<T: Action>() { T::run(); } pub fn main() { invoke::<Used>(); }").await;
    assert!(checked.diagnostics.is_empty());
    assert!(
        checked.sig.effects.unknown,
        "source alone cannot prove purity"
    );
    checked
        .apply_driver_effects_json(&output(&["sleep"], serde_json::json!([])))
        .unwrap();
    assert!(checked.diagnostics.is_empty());
    assert_eq!(checked.sig.effects.labels, ["sleep"]);
    assert!(!checked.sig.effects.unknown);
}

#[tokio::test]
async fn unknown_label_error_names_call_site() {
    let mut checked = check("pub fn main(label: String) { loom::perform(label, ()); }").await;
    checked
        .apply_driver_effects_json(&output(
            &[],
            serde_json::json!([
                {"item":"helper::dispatch", "span":"src/helper.rs:17:9"}
            ]),
        ))
        .unwrap();
    let error = &checked.diagnostics[0];
    assert!(
        error
            .message
            .contains("rows are inferred and need a static label")
    );
    assert!(
        error
            .message
            .contains("effect label at src/helper.rs:17:9 is not a literal or const")
    );
    assert_eq!(
        error.message,
        "effect label at src/helper.rs:17:9 is not a literal or const; rows are inferred and need a static label"
    );
    assert_eq!(error.file, "src/helper.rs");
    assert_eq!(error.line, 17);
    assert_eq!(error.col, 9);
}

#[tokio::test]
async fn missing_driver_rows_never_become_pure() {
    let mut checked = check("pub fn main() {}").await;
    assert!(checked.apply_driver_effects_json("{}").is_err());
    assert!(
        checked
            .apply_driver_effects_json(r#"{"effects":{"entries":{},"instances":{}}}"#)
            .is_err()
    );
    assert!(checked.sig.effects.unknown);
}

#[tokio::test]
async fn multiple_root_functions_export_but_nested_functions_do_not() {
    let mut checked = check("pub fn main() {} pub fn other() {} fn private() {} pub(crate) fn internal() {} mod nested { pub fn hidden() {} }").await;
    assert!(checked.diagnostics.is_empty());
    assert_eq!(
        checked
            .sig
            .exports
            .iter()
            .map(|export| export.name.as_str())
            .collect::<Vec<_>>(),
        ["main", "other"]
    );
    checked.apply_driver_effects_json(r#"{"effects":{"entries":{"guest::main":{"labels":["now"],"unknown":[]},"guest::other":{"labels":[],"unknown":[]}},"instances":{}}}"#).unwrap();
    assert!(checked.diagnostics.is_empty());
    assert_eq!(checked.sig.effects.labels, ["now"]);
}

#[tokio::test]
async fn missing_entry_is_rejected_without_mutation() {
    let mut checked = check("pub fn main() {}").await;
    let before = serde_json::to_value(&checked).unwrap();
    let error = checked
        .apply_driver_effects_json(
            r#"{"effects":{"entries":{"other":{"labels":[],"unknown":[]}},"instances":{}}}"#,
        )
        .unwrap_err();
    assert!(error.to_string().contains("main"));
    assert_eq!(serde_json::to_value(&checked).unwrap(), before);
}

#[tokio::test]
async fn resolving_all_labels_clears_previous_call_site_errors() {
    let mut checked = check("pub fn main(label: &str) { loom::perform(label, ()); }").await;
    checked
        .apply_driver_effects_json(&output(
            &["sleep"],
            serde_json::json!([
                {"item":"main", "span":"src/lib.rs:1:1"},
                {"item":"helper", "span":"src/helper.rs:3:4"}
            ]),
        ))
        .unwrap();
    assert_eq!(checked.diagnostics.len(), 2);
    assert_eq!(checked.sig.effects.labels, ["sleep"]);
    checked
        .apply_driver_effects_json(&output(&["now", "sleep"], serde_json::json!([])))
        .unwrap();
    assert!(checked.diagnostics.is_empty());
    assert!(!checked.sig.effects.unknown);
    assert_eq!(checked.sig.effects.labels, ["now", "sleep"]);
}

#[tokio::test]
async fn definition_requires_a_crate_root_public_function() {
    for source in [
        "fn private() {}",
        "mod nested { pub fn hidden() {} }",
        "pub(crate) fn internal() {}",
        "pub struct Data;",
        "",
    ] {
        let checked = check(source).await;
        let error = checked
            .diagnostics
            .iter()
            .find(|error| error.code == "LOOM_ENTRYPOINT")
            .unwrap_or_else(|| panic!("missing entry diagnostic for {source:?}"));
        assert!(error.message.contains("crate-root pub fn"));
        assert!(checked.sig.exports.is_empty());
    }
}

#[tokio::test]
async fn helper_bundle_files_do_not_require_entries() {
    let source = serde_json::json!({"files": {
        "Cargo.toml": "[package]\nname='guest'\nversion='0.1.0'\n",
        "src/lib.rs": "mod helper; pub fn main() {}",
        "src/helper.rs": "fn private() {}"
    }})
    .to_string();
    let checked = check(&source).await;
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    assert_eq!(checked.sig.exports.len(), 1);
}
