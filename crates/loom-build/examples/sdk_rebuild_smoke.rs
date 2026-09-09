//! Rebuild an immutable definition prepared with a separately archived SDK.
use loom_build::Builder;
use loom_check::{Checker, SourceBundle};
use loom_proto::{DefineRequest, Lang};
use std::{collections::BTreeMap, path::PathBuf};

fn lock_text(source: &str) -> String {
    let bundle: SourceBundle = serde_json::from_str(source).expect("source bundle");
    bundle.files["Cargo.lock"]
        .as_text()
        .expect("lock text")
        .to_owned()
}
fn version(lock: &str, name: &str) -> String {
    let lock: toml::Value = toml::from_str(lock).expect("lock TOML");
    lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("missing package {name}"))["version"]
        .as_str()
        .unwrap()
        .to_owned()
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().ok_or("missing current root")?);
    let legacy = PathBuf::from(args.next().ok_or("missing legacy root")?);
    let output = PathBuf::from(args.next().ok_or("missing component output")?);
    let cache = PathBuf::from(std::env::var_os("LOOM_BUILD_DIR").ok_or("set LOOM_BUILD_DIR")?);
    let checker = Checker::new(root.clone());
    let mut request = DefineRequest {
        lang: Lang::Rust,
        name: "legacy_render".into(),
        source: std::fs::read_to_string(root.join("examples/bundles/itoa.json"))?,
        deps: BTreeMap::new(),
    };
    let initial = checker.check(&request).await?;
    assert!(initial.diagnostics.is_empty(), "{:?}", initial.diagnostics);
    request.source = Builder::new(legacy)
        .prepare_rust_source(&initial, &BTreeMap::new())
        .await?;
    let checked = checker.check(&request).await?;
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let original_source = checked.source.clone();
    let original_hash = checked.hash.clone();
    let old_lock = lock_text(&original_source);
    assert!(old_lock.contains("name = \"ciborium\""));
    assert!(!old_lock.contains("name = \"serde_ipld_dagcbor\""));
    let built = Builder::new(root).build(&checked).await?;
    assert!(built.diagnostics.is_empty(), "{:?}", built.diagnostics);
    assert_eq!(checked.hash, original_hash);
    assert_eq!(checked.source, original_source);
    let new_lock = std::fs::read_to_string(cache.join(&checked.hash).join("Cargo.lock"))?;
    assert_eq!(version(&old_lock, "itoa"), version(&new_lock, "itoa"));
    assert_eq!(version(&new_lock, "serde_ipld_dagcbor"), "0.7.0");
    std::fs::write(output, &built.component)?;
    println!(
        "{}",
        serde_json::json!({"passed":1,"total":1,"hash":checked.hash,"user_pin":version(&new_lock,"itoa"),"codec":version(&new_lock,"serde_ipld_dagcbor"),"size":built.component.len(),"ms":built.ms})
    );
    Ok(())
}
