use super::*;
#[test]
fn detached_spawn_closure_effects_flow_into_caller() {
    let row = infer_source(
        "#[loom::def] fn main() { loom::spawn(|| { loom::sleep(1); loom::now(); }); }",
    );
    assert_eq!(row.labels, vec!["now", "sleep"]);
    assert!(!row.unknown);
    let alias = infer_source(
        "use loom::spawn as start; #[loom::def] fn main() { start(|| loom::sleep(1)); }",
    );
    assert_eq!(alias.labels, vec!["sleep"]);
    assert!(!alias.unknown);
}
fn infer_source(source: &str) -> EffectSet {
    infer(&syn::parse_file(source).unwrap(), &BTreeMap::new())
        .remove("main")
        .unwrap()
}
#[test]
fn labeled_handlers_discharge_body_helpers_but_not_handler_effects() {
    let row = infer_source(
        r#"
            fn read() { loom::fs::read("local", "."); }
            fn main() {
                loom::sleep(1);
                loom::handle(["fs.read"], |op, k| { loom::now(); }, || read());
            }
        "#,
    );
    assert_eq!(row.labels, vec!["now", "sleep"]);
    assert!(!row.unknown);
    let row = infer_source(
        r#"fn main() {
            loom::handle_any(|op,k| {}, || loom::fs::read("local", "."));
        }"#,
    );
    assert_eq!(row.labels, vec!["fs.read"]);
}

#[test]
fn handler_function_values_contribute_outer_effects() {
    let row = infer_source(
        r#"
            fn handler() { loom::now(); }
            fn body() { loom::sleep(1); }
            fn main() { loom::handle(["sleep"], handler, body); }
        "#,
    );
    assert_eq!(row.labels, vec!["now"]);
    assert!(!row.unknown);
    assert!(infer_source("fn main() { loom::handle_any(external::handler, || 1); }").unknown);
}

#[test]
fn residual_declaration_names_unhandled_labels() {
    let bad = syn::parse_file(
        r#"#[loom::def(effects=["sleep"])] fn main() {
            loom::sleep(1); loom::fs::read("local", ".");
        }"#,
    )
    .unwrap();
    let diagnostics = declaration_diagnostics(&bad, &infer(&bad, &BTreeMap::new()));
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, "LOOM_EFFECT_ROW");
    let good = syn::parse_file(
        r#"#[loom::def(effects=["sleep"])] fn main() {
            loom::sleep(1);
            loom::handle(["fs.read"], |op,k| {}, || loom::fs::read("local", "."));
        }"#,
    )
    .unwrap();
    let rows = infer(&good, &BTreeMap::new());
    assert!(declaration_diagnostics(&good, &rows).is_empty());
    assert_eq!(rows["main"].declared, Some(vec!["sleep".into()]));
    let dynamic = syn::parse_file("#[loom::def] fn main() { callback(); }").unwrap();
    assert_eq!(
        declaration_diagnostics(&dynamic, &infer(&dynamic, &BTreeMap::new())).len(),
        1
    );
    let explicit = syn::parse_file("#[loom::def(effects=[])] fn main() { callback(); }").unwrap();
    let rows = infer(&explicit, &BTreeMap::new());
    assert!(rows["main"].unknown);
    assert!(declaration_diagnostics(&explicit, &rows).is_empty());
}

#[test]
fn nested_handler_rows_and_unknown_dispatch_stay_conservative() {
    let row = infer_source(
        r#"fn work() { loom::fs::read("local", "."); }
            fn main() {
                work();
                loom::handle(["fs.read"], |op,k| {}, || work());
            }"#,
    );
    assert_eq!(row.labels, vec!["fs.read"]);
    let row = infer_source(
        r#"fn main() {
            loom::handle(["fs.read"], |op,k| {}, || callback());
        }"#,
    );
    assert!(row.unknown);
    let row = infer_source(
        r#"fn main() {
            loom::handle(labels, |op,k| {}, || loom::fs::read("local", "."));
        }"#,
    );
    assert_eq!(row.labels, vec!["fs.read"]);
    assert!(row.unknown);
}

#[test]
fn primitive_arithmetic_is_known_and_helpers_propagate() {
    assert_eq!(
        infer_source("fn main(a:i64)->i64 {a+1}"),
        EffectSet {
            labels: vec![],
            unknown: false,
            declared: None
        }
    );
    let effects =
        infer_source("use loom::now as clock; fn helper(){clock();} fn main(){helper();}");
    assert_eq!(
        effects,
        EffectSet {
            labels: vec!["now".into()],
            unknown: false,
            declared: None
        }
    );
}
#[test]
fn perform_literal_labels_are_effects_and_dynamic_labels_stay_unknown() {
    let effects = infer_source(r#"fn main(){loom::perform::<u64>("exec", 0);}"#);
    assert_eq!(effects.labels, vec!["exec"]);
    assert!(!effects.unknown);
    let effects = infer_source(r#"fn main(){loom::perform("call", 0);}"#);
    assert_eq!(effects.labels, vec!["call"]);
    assert!(effects.unknown);
    assert!(infer_source("fn main(){loom::perform(label, 0);}").unknown);
    assert!(infer_source("fn main(){callback();}").unknown);
}
#[test]
fn shadowed_names_custom_traits_and_macros_are_not_claimed_pure() {
    let effects = infer_source("use loom::now as clock; fn main(clock:fn()){clock();}");
    assert!(effects.unknown);
    assert!(effects.labels.is_empty());
    assert!(
        infer_source(
            "struct S; impl Drop for S {fn drop(&mut self){loom::random();}} fn main(){let _x=S;}"
        )
        .unknown
    );
    assert!(infer_source("fn main(){custom!();}").unknown);
    assert!(infer_source("fn main(){fn Ok(){loom::now();} Ok();}").unknown);
    assert!(infer_source("use external::*; fn main(){Ok();}").unknown);
}
#[test]
fn known_dependency_effects_propagate_through_call() {
    let sig:TypeSig=serde_json::from_value(serde_json::json!({"effects":{"labels":["llm"],"unknown":false},"exports":[{"name":"work","params":[],"returns":{"type":"null"},"effects":{"labels":["llm"],"unknown":false}}]})).unwrap();
    let mut signatures = BTreeMap::new();
    signatures.insert("worker".into(), sig);
    let effects = infer(
        &syn::parse_file("fn main(){loom::call(worker::WORK_DEF,0);}").unwrap(),
        &signatures,
    )
    .remove("main")
    .unwrap();
    assert_eq!(
        effects,
        EffectSet {
            labels: vec!["call".into(), "llm".into()],
            unknown: false,
            declared: None,
        }
    );
}
