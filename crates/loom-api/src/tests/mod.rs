mod cas;
mod transport;
use super::*;
#[tokio::test]
async fn response_refuses_success_when_recording_cannot_commit() -> Result<()> {
    let store = Store::memory()?;
    let service = Service::new(
        store.clone(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )?;
    store.with_connection(|connection| {
            connection.execute_batch("CREATE TRIGGER refuse_recording BEFORE INSERT ON definition_records BEGIN SELECT RAISE(ABORT, 'recording control'); END;")?;
            Ok(())
        })?;
    store.enqueue_recording(&json!({"type":"effect_invoked","op":"sleep"}))?;
    let response = service.response(Ok(json!(42)));
    assert!(!response.ok);
    assert_eq!(response.result["code"], "store_unavailable");
    assert!(
        response.result["error"]
            .as_str()
            .unwrap()
            .contains("recording control")
    );
    Ok(())
}
use tower::ServiceExt;
fn app() -> Router {
    router(
        Arc::new(
            Service::new(
                Store::memory().unwrap(),
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
                vec![Lang::Rust],
            )
            .unwrap(),
        ),
        Authorizer::single("test-secret".into()).unwrap(),
    )
}
#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER pointing to loomd or build_smoke"]
async fn explicit_effect_policy_persists_and_changes_identity() {
    let service = Service::new(
        Store::memory().unwrap(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )
    .unwrap();
    let request = DefineRequest {
        lang: Lang::Rust,
        name: "policy".into(),
        source: "pub fn main() -> i32 { 42 }".into(),
        deps: BTreeMap::new(),
        allowed_effects: Some(Vec::new()),
    };
    let restricted = service.define(request.clone()).await;
    assert!(restricted.ok, "{restricted:?}");
    let hash = restricted.result["def"]["hash"].as_str().unwrap();
    assert_eq!(
        service
            .store
            .definition(hash)
            .unwrap()
            .unwrap()
            .allowed_effects,
        Some(Vec::new())
    );
    let unrestricted = service
        .define(DefineRequest {
            allowed_effects: None,
            ..request
        })
        .await;
    assert!(unrestricted.ok, "{unrestricted:?}");
    assert_ne!(
        restricted.result["def"]["hash"],
        unrestricted.result["def"]["hash"]
    );
}
#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER pointing to loomd or build_smoke"]
async fn redefinition_preserves_pins() {
    let service = Service::new(
        Store::memory().unwrap(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )
    .unwrap();
    let first = service
        .define(DefineRequest {
            allowed_effects: None,
            lang: Lang::Rust,
            name: "add".into(),
            source: "pub fn main(x:i32)->i32 { x+1 }".into(),
            deps: BTreeMap::new(),
        })
        .await;
    assert!(first.ok, "{first:?}");
    let hash = first.result["def"]["hash"].as_str().unwrap().to_owned();
    let mut deps = BTreeMap::new();
    deps.insert("add".into(), hash);
    let mut dependent = DefineRequest {
        allowed_effects: None,
        lang: Lang::Rust,
        name: "caller".into(),
        source: "pub fn main()->i32 { add::main(41) }".into(),
        deps,
    };
    let before = service.define(dependent.clone()).await;
    assert!(before.ok, "{before:?}");
    dependent.name = "bad-caller".into();
    dependent.source = dependent.source.replace("(41)", "(true)");
    let rejected = service.define(dependent).await;
    assert!(!rejected.ok, "{rejected:?}");
    let second = service
        .define(DefineRequest {
            allowed_effects: None,
            lang: Lang::Rust,
            name: "add".into(),
            source: "pub fn main(x:i32)->i32 { x+2 }".into(),
            deps: BTreeMap::new(),
        })
        .await;
    assert!(second.ok, "{second:?}");
    assert_eq!(
        service.store.resolve("caller").unwrap().unwrap().hash,
        before.result["def"]["hash"].as_str().unwrap()
    );
}
#[test]
fn rust_source_is_not_a_bundle_reference() {
    assert_eq!(source_reference("pub fn add(x:i32)->i32{x+1}"), None);
    assert_eq!(
        source_reference("#![allow(dead_code)]\npub fn main() {}"),
        None
    );
    let reference = format!("#{}", "a".repeat(64));
    assert_eq!(source_reference(&reference), Some(&reference[1..]));
}
#[tokio::test]
async fn missing_source_archive_reports_missing_reference() {
    let service = Service::new(
        Store::memory().unwrap(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )
    .unwrap();
    let response = service
        .define(DefineRequest {
            allowed_effects: None,
            lang: Lang::Rust,
            name: "missing".into(),
            source: format!("#{}", "a".repeat(64)),
            deps: BTreeMap::new(),
        })
        .await;
    assert!(!response.ok);
    assert!(
        response.result["error"]
            .as_str()
            .unwrap()
            .contains(&format!("Rust source bundle {} not found", "a".repeat(64)))
    );
    let source = "pub fn main() { std::fs::read(\"secret\").unwrap(); }";
    let checked = service
        .define(DefineRequest {
            allowed_effects: None,
            lang: Lang::Rust,
            name: "macro".into(),
            source: source.into(),
            deps: BTreeMap::new(),
        })
        .await;
    assert!(!checked.ok);
    assert!(!checked.diagnostics.is_empty(), "{checked:?}");
}
#[tokio::test]
async fn read_scope_cannot_execute_or_define_through_service_or_http() {
    let service = Arc::new(
        Service::new(
            Store::memory().unwrap(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            vec![Lang::Rust],
        )
        .unwrap(),
    );
    let authorizer = Authorizer::new(vec![TokenConfig {
        token: "reader".into(),
        scopes: [Scope::Read].into_iter().collect(),
    }])
    .unwrap();
    let reader = service.scoped(authorizer.authenticate("reader").unwrap());
    let response = reader
        .command(CommandRequest {
            session: None,
            command: "run".into(),
            args: json!({"target":"missing"}),
        })
        .await;
    assert!(!response.ok);
    assert_eq!(response.result["code"], "forbidden");
    let call = reader
        .command(CommandRequest {
            session: None,
            command: "call".into(),
            args: json!({"hash":"missing","args":[]}),
        })
        .await;
    assert_eq!(call.result["code"], "forbidden");
    assert_eq!(service.store.latest_seq().unwrap(), 0);
    let response = router(service, authorizer)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/command")
                .header("authorization", "Bearer reader")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"command":"add","args":{"name":"denied","source":"pub fn main() {}"}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
#[test]
fn source_archive_preserves_binary_assets_and_rejects_escape_paths() {
    fn append(builder: &mut tar::Builder<Vec<u8>>, path: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
        header.set_cksum();
        builder.append(&header, bytes).unwrap();
    }
    let mut archive = tar::Builder::new(Vec::new());
    append(
        &mut archive,
        "Cargo.toml",
        b"[package]\nname='asset'\nversion='0.1.0'\n",
    );
    append(&mut archive, "src/lib.rs", b"pub fn answer()->u32 {42}");
    append(&mut archive, "assets/raw", &[0xff, 0, 0xfe]);
    let bytes = archive.into_inner().unwrap();
    let decoded: loom_check::SourceBundle =
        serde_json::from_str(&decode_source_bundle(&bytes).unwrap()).unwrap();
    assert_eq!(
        decoded.files["assets/raw"].bytes().unwrap(),
        vec![0xff, 0, 0xfe]
    );
    let mut escaped = tar::Builder::new(Vec::new());
    append(&mut escaped, "../outside", b"bad");
    assert!(
        decode_source_bundle(&escaped.into_inner().unwrap())
            .unwrap_err()
            .to_string()
            .contains("invalid source archive path")
    );
}
