use super::*;
#[tokio::test]
async fn rust_rejects_ambient_io_and_hashes_formatting_stably() {
    let checker = Checker::new();
    let mut request = DefineRequest {
        lang: Lang::Rust,
        name: "test".into(),
        source: "pub fn f()->u32 { 2 }".into(),
        deps: BTreeMap::new(),
        allowed_effects: None,
    };
    let first = checker.check(&request).await.unwrap();
    request.source = "pub fn f() -> u32 {\n 2\n}\n".into();
    assert_eq!(first.hash, checker.check(&request).await.unwrap().hash);
    request.allowed_effects = Some(vec![]);
    assert_ne!(first.hash, checker.check(&request).await.unwrap().hash);
    request.source = "pub fn f() { std::fs::read(\"secret\").unwrap(); }".into();
    assert!(
        !checker
            .check(&request)
            .await
            .unwrap()
            .diagnostics
            .is_empty()
    );
}
#[tokio::test]
async fn external_crate_initialization_is_not_claimed_pure() {
    let request=DefineRequest {
            lang:Lang::Rust,name:"external".into(),deps:BTreeMap::new(),allowed_effects:None,
            source:serde_json::json!({"files":{"Cargo.toml":"[package]\nname='external'\nversion='0.1.0'\n[dependencies]\nthird_party='1'\n","src/lib.rs":"pub fn main()->i64 {42}"}}).to_string(),
        };
    let checked = Checker::new().check(&request).await.unwrap();
    assert!(checked.diagnostics.is_empty());
    assert!(checked.sig.effects.unknown);
    assert!(checked.sig.exports[0].effects.unknown);
}
#[tokio::test]
async fn rejects_compiler_file_reads_and_macro_aliases() {
    let checker = Checker::new();
    for source in [
        r#"pub fn f()->String { include_str!("/etc/passwd").into() }"#,
        r#"pub fn f()->String { include_str!("/tmp/secret").into() }"#,
        r#"pub fn f()->String { env!("LOOM_TOKEN").into() }"#,
        r#"use core::include_str as secret; pub fn f()->String {secret!("/etc/passwd").into()}"#,
        r#"#[cfg_attr(all(),path="/etc/passwd")]mod secret;"#,
        r#"use std::{fs as files}; pub fn f(){let _=files::read("/tmp/x");}"#,
    ] {
        let request = DefineRequest {
            lang: Lang::Rust,
            name: "test".into(),
            source: source.into(),
            deps: BTreeMap::new(),
            allowed_effects: None,
        };
        assert!(
            checker
                .check(&request)
                .await
                .unwrap()
                .diagnostics
                .iter()
                .any(|error| error.code == "LOOM_IO"),
            "{source}"
        );
    }
}
#[test]
fn bundle_bytes_roundtrip_and_paths_are_bounded() {
    let original = vec![0, 255, 128, 1];
    assert_eq!(
        SourceFile::from_bytes(original.clone()).bytes().unwrap(),
        original
    );
    let mut files = BTreeMap::new();
    files.insert("Cargo.toml".into(), SourceFile::Text("[package]".into()));
    files.insert("src/lib.rs".into(), SourceFile::Text(String::new()));
    files.insert("../escape".into(), SourceFile::Text(String::new()));
    assert!(SourceBundle { files }.validate().is_err());
}
#[tokio::test]
async fn macros_expand_inside_rustc_so_builtin_forms_pass_and_procedural_forms_fail() {
    let checker = Checker::new();
    let request = |source: &str| DefineRequest {
        lang: Lang::Rust,
        name: "test".into(),
        source: source.into(),
        deps: BTreeMap::new(),
        allowed_effects: None,
    };
    let accepted = checker
        .check(&request(
            "#[derive(Clone, Copy, PartialEq)] struct Square(f64); pub fn area(square: Square) -> String { let sides = vec![square.0, square.0]; assert!(square == square.clone()); format!(\"{}\", sides.iter().product::<f64>()) }",
        ))
        .await
        .unwrap();
    assert!(
        accepted.diagnostics.is_empty(),
        "{:?}",
        accepted.diagnostics
    );
    assert_eq!(accepted.sig.exports[0].name, "area");
    let rejected = checker
        .check(&request(
            "#[derive(serde::Serialize)] struct Square(f64); pub fn main() {}",
        ))
        .await
        .unwrap();
    let error = rejected
        .diagnostics
        .iter()
        .find(|error| error.code == "LOOM_MACRO")
        .expect("procedural derive is refused");
    assert!(
        error.message.contains("serde::Serialize"),
        "{}",
        error.message
    );
    // A macro body is tokens to the source passes; ambient input inside it is
    // still refused, by LOOM_IO, without the macro itself being refused.
    let smuggled = checker
        .check(&request(
            "macro_rules! secret { () => { std::fs::read(\"x\") } } pub fn main() { secret!(); }",
        ))
        .await
        .unwrap();
    assert!(
        smuggled
            .diagnostics
            .iter()
            .any(|error| error.code == "LOOM_IO" && error.message.contains("std::fs")),
        "{:?}",
        smuggled.diagnostics
    );
    assert!(
        smuggled
            .diagnostics
            .iter()
            .all(|error| error.code != "LOOM_MACRO"),
        "{:?}",
        smuggled.diagnostics
    );
    // A local merely named like an ambient macro is not one.
    let named = checker
        .check(&request("pub fn main(env: u8) -> Vec<u8> { vec![env] }"))
        .await
        .unwrap();
    assert!(named.diagnostics.is_empty(), "{:?}", named.diagnostics);
}
/// A `macro_rules!` transcriber may assemble a macro invocation, an attribute
/// or a renamed import out of fragments that neither side spells in full. Each
/// route is refused and the diagnostic names the fragment or alias; a body
/// that spells `vec!` and `format!` itself, and an alias outside the tables,
/// are accepted.
#[tokio::test]
async fn macro_fragments_and_table_aliases_cannot_smuggle_refused_items() {
    let checker = Checker::new();
    let request = |source: &str| DefineRequest {
        lang: Lang::Rust,
        name: "test".into(),
        source: source.into(),
        deps: BTreeMap::new(),
        allowed_effects: None,
    };
    // B1: the macro name arrives through a fragment; refused by LOOM_MACRO and
    // by LOOM_IO, both naming the fragment.
    let fragment = checker
        .check(&request(
            "macro_rules! call { ($m:ident) => { $m!(\"/etc/passwd\") } } pub fn main() -> &'static str { call!(include_str) }",
        ))
        .await
        .unwrap();
    assert!(
        fragment
            .diagnostics
            .iter()
            .any(|error| error.code == "LOOM_MACRO" && error.message.contains("Fragment `$m`")),
        "{:?}",
        fragment.diagnostics
    );
    assert!(
        fragment
            .diagnostics
            .iter()
            .any(|error| error.code == "LOOM_IO" && error.message.contains("fragment `$m`")),
        "{:?}",
        fragment.diagnostics
    );
    let repetition = checker
        .check(&request(
            "macro_rules! call { ($($m:ident)*) => { $($m)*!(\"LOOM_TOKEN\") } } pub fn main() -> &'static str { call!(env) }",
        ))
        .await
        .unwrap();
    assert!(
        repetition
            .diagnostics
            .iter()
            .any(|error| error.code == "LOOM_MACRO" && error.message.contains("Fragment `$(...)*`")),
        "{:?}",
        repetition.diagnostics
    );
    assert!(
        repetition
            .diagnostics
            .iter()
            .any(|error| error.code == "LOOM_IO" && error.message.contains("fragment `$(...)`")),
        "{:?}",
        repetition.diagnostics
    );
    // B2: the attribute arrives through a `meta` fragment.
    for source in [
        "macro_rules! tag { ($a:meta, $i:item) => { #[$a] $i } } tag!(cfg(not(test)), pub fn hidden() {}); pub fn main() {}",
        "macro_rules! tag { ($a:meta, $i:item) => { #[$a] $i } } tag!(derive(loom::serde::Serialize), struct S;); pub fn main() {}",
    ] {
        let tagged = checker.check(&request(source)).await.unwrap();
        assert!(
            tagged
                .diagnostics
                .iter()
                .any(|error| error.code == "LOOM_MACRO" && error.message.contains("Fragment `$a`")),
            "{source}: {:?}",
            tagged.diagnostics
        );
    }
    // A `tt` fragment can carry a bare `#`, `!` or `[...]`; refused by
    // specifier.
    let token_tree = checker
        .check(&request(
            "macro_rules! call { ($bang:tt) => { include_str $bang (\"/etc/passwd\") } } pub fn main() { call!(!); }",
        ))
        .await
        .unwrap();
    assert!(
        token_tree.diagnostics.iter().any(
            |error| error.code == "LOOM_MACRO" && error.message.contains("Fragment `$bang:tt`")
        ),
        "{:?}",
        token_tree.diagnostics
    );
    // B3: a renamed import would pass the tables by spelling.
    for (source, alias) in [
        (
            "use loom::serde::Serialize as Clone; #[derive(Clone)] struct S; pub fn main() {}",
            "`loom::serde::Serialize as Clone`",
        ),
        (
            "use std::println as format; pub fn main() { format!(\"x\"); }",
            "`std::println as format`",
        ),
    ] {
        let renamed = checker.check(&request(source)).await.unwrap();
        assert!(
            renamed
                .diagnostics
                .iter()
                .any(|error| error.code == "LOOM_MACRO" && error.message.contains(alias)),
            "{source}: {:?}",
            renamed.diagnostics
        );
    }
    // Positive controls: a body that spells its macros, and an alias outside
    // every table.
    for source in [
        "macro_rules! pair { ($a:expr) => { vec![format!(\"{}\", $a), format!(\"{}\", $a)] } } pub fn main() -> Vec<String> { pair!(1) }",
        "use std::fmt::Write as FmtWrite; pub fn main() -> String { let mut out = String::new(); write!(out, \"{}\", 1).unwrap(); out }",
    ] {
        let accepted = checker.check(&request(source)).await.unwrap();
        assert!(
            accepted.diagnostics.is_empty(),
            "{source}: {:?}",
            accepted.diagnostics
        );
    }
}
