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
