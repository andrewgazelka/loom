use super::*;
#[tokio::test]
async fn crate_pins_materialize_but_caller_paths_and_overlays_are_rejected() {
    let store = loom_store::Store::memory().unwrap();
    let manifest_hash = store
        .put("blob", b"[package]\nname='tiny'\nversion='1.0.0'\n")
        .unwrap();
    let tree = loom_proto::Tree {
        entries: vec![loom_proto::TreeEntry {
            name: "Cargo.toml".into(),
            reference: store
                .reference(&manifest_hash, loom_proto::RAW_CODEC)
                .unwrap(),
            directory: false,
            executable: false,
        }],
    };
    let hash = store.put_value("tree", &tree).unwrap();
    let manifest = format!(
        "[package]\nname='loom-definition'\nversion='0.1.0'\nedition='2024'\n[loom.crates]\ntiny={{hash='{hash}'}}\n"
    );
    let mut files = BTreeMap::new();
    files.insert("Cargo.toml".into(), SourceFile::Text(manifest));
    files.insert(
        "src/lib.rs".into(),
        SourceFile::Text("pub fn main() -> i64 { 42 }".into()),
    );
    let mut bundle = SourceBundle { files };
    let mut definition = CheckedDef {
        hash: "a".repeat(64),
        lang: Lang::Rust,
        name: "test".into(),
        source: serde_json::to_string(&bundle).unwrap(),
        deps: BTreeMap::new(),
        sig: Default::default(),
        diagnostics: vec![],
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let directory = std::env::temp_dir().join(format!("loom-crate-paths-{}", std::process::id()));
    if directory.exists() {
        fs::remove_dir_all(&directory).await.unwrap();
    }
    materialize_rust(Materialization {
        store: &store,
        root: &root,
        cache: &directory,
        directory: &directory,
        definition: &definition,
        dependencies: &BTreeMap::new(),
        dependency: false,
        isolated: true,
    })
    .await
    .unwrap();
    let emitted: toml::Value = fs::read_to_string(directory.join("Cargo.toml"))
        .await
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        emitted["dependencies"]["tiny"]["version"].as_str(),
        Some("=1.0.0")
    );
    assert!(emitted["dependencies"]["tiny"].get("path").is_none());
    assert_eq!(
        emitted["patch"]["crates-io"][format!("loom-pin-{hash}")]["path"].as_str(),
        Some(format!("loom-crates/{hash}").as_str())
    );
    assert_eq!(
        fs::read(directory.join(format!("loom-crates/{hash}/Cargo.toml")))
            .await
            .unwrap(),
        store.get(&manifest_hash).unwrap().unwrap()
    );
    bundle.files.insert(
        format!("loom-crates/{hash}/Cargo.toml"),
        SourceFile::Text("tampered".into()),
    );
    definition.source = serde_json::to_string(&bundle).unwrap();
    assert!(
        materialize_rust(Materialization {
            store: &store,
            root: &root,
            cache: &directory,
            directory: &directory,
            definition: &definition,
            dependencies: &BTreeMap::new(),
            dependency: false,
            isolated: true,
        })
        .await
        .unwrap_err()
        .to_string()
        .contains("host-owned")
    );
    bundle
        .files
        .remove(&format!("loom-crates/{hash}/Cargo.toml"));
    bundle.files.insert("Cargo.toml".into(), SourceFile::Text(format!("[package]\nname='loom-definition'\nversion='0.1.0'\n[dependencies]\ntiny={{path='loom-crates/{hash}'}}\n")));
    definition.source = serde_json::to_string(&bundle).unwrap();
    assert!(
        materialize_rust(Materialization {
            store: &store,
            root: &root,
            cache: &directory,
            directory: &directory,
            definition: &definition,
            dependencies: &BTreeMap::new(),
            dependency: false,
            isolated: true,
        })
        .await
        .unwrap_err()
        .to_string()
        .contains("not path/git")
    );
    bundle.files.insert("Cargo.toml".into(), SourceFile::Text(format!("[package]\nname='loom-definition'\nversion='0.1.0'\n[patch.crates-io]\ntiny={{path='loom-crates/{hash}'}}\n")));
    definition.source = serde_json::to_string(&bundle).unwrap();
    assert!(
        materialize_rust(Materialization {
            store: &store,
            root: &root,
            cache: &directory,
            directory: &directory,
            definition: &definition,
            dependencies: &BTreeMap::new(),
            dependency: false,
            isolated: true,
        })
        .await
        .unwrap_err()
        .to_string()
        .contains("overrides are unavailable")
    );
    fs::remove_dir_all(directory).await.unwrap();
}
#[cfg(unix)]
#[tokio::test]
async fn immutable_sdk_lock_becomes_mutable_build_input() {
    use std::os::unix::fs::PermissionsExt;
    let directory = std::env::temp_dir().join(format!("loom-readonly-lock-{}", std::process::id()));
    fs::create_dir_all(&directory).await.unwrap();
    let source = directory.join("sdk.lock");
    let destination = directory.join("build.lock");
    fs::write(&source, b"version = 4\n").await.unwrap();
    fs::set_permissions(&source, std::fs::Permissions::from_mode(0o444))
        .await
        .unwrap();
    seed_build_lock(&source, &destination).await.unwrap();
    assert_ne!(
        fs::metadata(&destination)
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o200,
        0
    );
    fs::write(&destination, b"updated build lock")
        .await
        .unwrap();
    assert_eq!(fs::read(&source).await.unwrap(), b"version = 4\n");
    assert_eq!(
        fs::metadata(&source).await.unwrap().permissions().mode() & 0o222,
        0
    );
    fs::remove_dir_all(directory).await.unwrap();
}
#[test]
fn cargo_artifact_uses_the_root_manifest_and_reported_filename() {
    let root = std::env::temp_dir().join(format!("loom-artifact-01a084e8-{}", std::process::id()));
    std::fs::create_dir_all(root.join("target")).unwrap();
    let manifest = root.join("Cargo.toml");
    std::fs::write(&manifest, "[package]").unwrap();
    let expected = root.join("target/custom_library.wasm");
    std::fs::write(&expected, b"new artifact").unwrap();
    std::fs::write(root.join("target/loom_definition.wasm"), b"stale artifact").unwrap();
    let message = serde_json::json!({"reason":"compiler-artifact","manifest_path":manifest,"target":{"crate_types":["cdylib"]},"filenames":[expected]});
    let actual = cargo_artifact(&message.to_string(), &manifest, &root.join("target")).unwrap();
    assert_eq!(std::fs::read(actual).unwrap(), b"new artifact");
    assert!(cargo_artifact("", &manifest, &root.join("target")).is_err());
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn rejects_unstamped_core_module() {
    assert!(validate_component(b"\0asm\x01\0\0\0").is_err());
}
#[test]
fn parses_cargo_error_amid_benign_warnings() {
    let diagnostics = cargo_diagnostics(
        "warning: unrelated\n{\"reason\":\"compiler-message\",\"message\":{\"level\":\"error\",\"message\":\"mismatched types\",\"code\":{\"code\":\"E0308\"},\"spans\":[],\"children\":[]}}\n",
    );
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, "E0308");
}

#[tokio::test]
async fn staged_builders_share_the_compiler_workspace_lock() {
    let store = loom_store::Store::memory().unwrap();
    let builder = Builder::new(PathBuf::from("."), store.clone());
    let staged = builder.for_store(store.stage_intake().unwrap());
    let guard = builder.gate.lock().await;
    assert!(staged.gate.try_lock().is_err());
    drop(guard);
    assert!(staged.gate.try_lock().is_ok());
}
