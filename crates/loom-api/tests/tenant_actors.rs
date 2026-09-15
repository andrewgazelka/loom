use anyhow::{Context, Result};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use loom_actor::{Behavior, Node, Rights};
use loom_api::{Access, Authorizer, Scope, Service, ServiceDirectory, TenantId, TokenConfig};
use loom_proto::{Def, Lang};
use loom_store::Store;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};
use tower::ServiceExt;

struct TenantFixture {
    service: Arc<Service>,
    node: Node,
    actor: String,
    blob: String,
}
struct Fixture {
    app: Router,
    alice: TenantFixture,
    bob: TenantFixture,
    directory: ServiceDirectory,
    authorizer: Authorizer,
    _directory: tempfile::TempDir,
}
async fn tenant(base: &std::path::Path, name: &str) -> Result<TenantFixture> {
    let store = Store::memory()?;
    let blob = store.put("blob", format!("private-{name}").as_bytes())?;
    let hash = store.put("source", format!("definition-{name}").as_bytes())?;
    store.define(
        &Def {
            hash,
            lang: Lang::Rust,
            component_hash: None,
            sig: Default::default(),
            allowed_effects: None,
            observed_effects: Vec::new(),
        },
        Some("definition"),
        name,
        &BTreeMap::new(),
    )?;
    let behavior: Arc<dyn Behavior> = Arc::new(loom_actor::builtin::Counter::plain());
    let service = Service::new(
        store,
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )?
    .with_tenant(TenantId::new(name)?)
    .with_build_directory(base.join(name).join("build"))
    .with_native_drivers(vec![behavior], Vec::new())?;
    let node = Node::new(
        base.join(name),
        service.actor_registry(),
        Arc::new(loom_actor::DefaultEffects),
        Default::default(),
    )
    .await?;
    let actor = node.spawn_root("counter-v1", b"").await?;
    node.register("worker", &actor).await?;
    Ok(TenantFixture {
        service: Arc::new(service.with_actors(node.clone())),
        node,
        actor,
        blob,
    })
}
async fn fixture() -> Result<Fixture> {
    let directory = tempfile::tempdir()?;
    let alice = tenant(directory.path(), "alice").await?;
    let bob = tenant(directory.path(), "bob").await?;
    let services = ServiceDirectory::new([alice.service.clone(), bob.service.clone()])?;
    let authorizer = Authorizer::new(
        ["alice", "bob"]
            .into_iter()
            .map(|name| TokenConfig {
                token: name.into(),
                tenant: TenantId::new(name).unwrap(),
                scopes: BTreeSet::from([Scope::Read, Scope::Execute, Scope::Define, Scope::Admin]),
            })
            .collect(),
    )?;
    let app = loom_api::router(services.clone(), authorizer.clone());
    Ok(Fixture {
        app,
        alice,
        bob,
        directory: services,
        authorizer,
        _directory: directory,
    })
}
async fn request(
    app: &Router,
    token: &str,
    method: &str,
    uri: &str,
    body: Value,
) -> Result<axum::response::Response> {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(if method == "GET" {
            Body::empty()
        } else {
            Body::from(serde_json::to_vec(&body)?)
        })?;
    Ok(app.clone().oneshot(request).await?)
}
async fn command(app: &Router, token: &str, name: &str, args: Value) -> Result<Value> {
    let response = request(
        app,
        token,
        "POST",
        "/v1/command",
        json!({"command":name,"args":args}),
    )
    .await?;
    Ok(serde_json::from_slice(
        &response.into_body().collect().await?.to_bytes(),
    )?)
}

#[tokio::test]
async fn same_actor_name_resolves_only_inside_authenticated_tenant() -> Result<()> {
    let f = fixture().await?;
    for name in ["alice", "bob"] {
        let result = command(&f.app, name, "whereis", json!({"name":"worker"})).await?;
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(
            result["result"],
            if name == "alice" {
                f.alice.actor.as_str()
            } else {
                f.bob.actor.as_str()
            }
        );
    }
    assert_ne!(f.alice.actor, f.bob.actor);
    Ok(())
}

#[tokio::test]
async fn actor_ids_cannot_cross_tenant_api_authority() -> Result<()> {
    let f = fixture().await?;
    for op in ["info", "send", "sql", "register"] {
        let args = match op {
            "send" => json!({"id":f.alice.actor,"msg":"intrusion"}),
            "sql" => json!({"id":f.alice.actor,"query":"SELECT * FROM entries"}),
            "register" => json!({"name":"stolen","id":f.alice.actor}),
            _ => json!({"id":f.alice.actor}),
        };
        let result = command(&f.app, "bob", op, args).await?;
        assert_eq!(result["ok"], false, "{op}: {result}");
    }
    let own = command(
        &f.app,
        "bob",
        "send",
        json!({"id":f.bob.actor,"msg":"owned"}),
    )
    .await?;
    assert_eq!(own["ok"], true, "{own}");
    let rows = f
        .bob
        .node
        .open(&f.bob.actor)
        .await?
        .inspect_sql("SELECT body FROM entries", Vec::new())
        .await?;
    assert!(rows.rows.iter().any(|row| {
        row.get::<Vec<u8>>(0)
            .is_ok_and(|value| value == b"\"owned\"")
    }));
    Ok(())
}

#[tokio::test]
async fn capabilities_and_service_scopes_do_not_cross_tenants() -> Result<()> {
    let f = fixture().await?;
    let cap = f.alice.node.cap_for(&f.alice.actor, Rights::SEND).await?;
    assert!(
        f.alice
            .node
            .check_cap(&cap, Rights::SEND, "test")
            .await
            .is_ok()
    );
    assert!(
        f.bob
            .node
            .check_cap(&cap, Rights::SEND, "test")
            .await
            .is_err()
    );
    assert!(
        f.alice
            .service
            .scoped(f.authorizer.authenticate("bob").context("auth missing")?)
            .is_err()
    );
    assert!(
        f.directory
            .scoped(Access::owner().for_tenant(TenantId::new("missing")?))
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn cas_and_definition_events_are_tenant_private() -> Result<()> {
    let f = fixture().await?;
    let path = format!("/v1/cas/{}", f.alice.blob);
    let own = request(&f.app, "alice", "GET", &path, Value::Null).await?;
    assert_eq!(own.status(), StatusCode::OK);
    assert!(
        own.headers()["cache-control"]
            .to_str()?
            .starts_with("private")
    );
    assert_eq!(
        request(&f.app, "bob", "GET", &path, Value::Null)
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    for name in ["alice", "bob"] {
        let response = request(&f.app, name, "GET", "/v1/events", Value::Null).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await?.to_bytes())?;
        let events = body["result"].as_array().context("events missing")?;
        let hash = if name == "alice" {
            f.alice.service.store.resolve("definition")?
        } else {
            f.bob.service.store.resolve("definition")?
        }
        .context("definition missing")?
        .hash;
        assert!(
            events.iter().any(|event| event.to_string().contains(&hash)),
            "{body}"
        );
        let other = if name == "alice" { &f.bob } else { &f.alice };
        assert!(
            !body.to_string().contains(
                &other
                    .service
                    .store
                    .resolve("definition")?
                    .context("definition missing")?
                    .hash
            )
        );
    }
    Ok(())
}

#[test]
fn tenant_ids_reject_path_aliases_and_config_defaults_are_explicit() -> Result<()> {
    for value in ["", "../alice", "alice/bob", "ALICE", ".", "a\\b", "λ"] {
        assert!(TenantId::new(value).is_err(), "{value}");
    }
    let token: TokenConfig = serde_json::from_value(json!({"token":"owner","scopes":["read"]}))?;
    assert_eq!(token.tenant, TenantId::default());
    Ok(())
}

#[tokio::test]
async fn websocket_stream_selects_tenant_before_events_or_capabilities() -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let f = fixture().await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let app = f.app.clone();
    struct Server(tokio::task::JoinHandle<()>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server = Server(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let connection = tokio_tungstenite::connect_async(format!("ws://{address}/v1/stream")).await?;
    let mut socket = connection.0;
    socket
        .send(Message::Text(json!({"token":"bob"}).to_string().into()))
        .await?;
    let ack = tokio::time::timeout(std::time::Duration::from_secs(3), socket.next())
        .await?
        .context("ack missing")??;
    assert_eq!(serde_json::from_str::<Value>(ack.to_text()?)?["ok"], true);
    let cap = f
        .alice
        .node
        .cap_for(&f.alice.actor, Rights::INSPECT)
        .await?;
    socket.send(Message::Text(json!({"subscribe":{"actor":f.alice.actor,"table":"entries","cap":serde_json::to_string(&cap)?}}).to_string().into())).await?;
    let alice_hash = f
        .alice
        .service
        .store
        .resolve("definition")?
        .context("alice definition missing")?
        .hash;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while let Some(message) = socket.next().await {
            let message = message?;
            if let Message::Text(text) = message {
                assert!(
                    !text.contains(&alice_hash),
                    "cross-tenant definition event: {text}"
                );
                let value: Value = serde_json::from_str(&text)?;
                if value.get("error").is_some() {
                    return Ok::<Value, anyhow::Error>(value);
                }
            }
        }
        anyhow::bail!("stream closed without refusing the foreign capability")
    })
    .await??;
    assert!(outcome["error"].is_string(), "{outcome}");
    Ok(())
}

#[tokio::test]
async fn tenant_cache_eviction_cannot_remove_sibling_build_workspace() -> Result<()> {
    let f = fixture().await?;
    let name = "a".repeat(64);
    let own = f.alice.service.build_directory().join(&name);
    let sibling = f.bob.service.build_directory().join(&name);
    assert_ne!(own, sibling);
    std::fs::create_dir_all(&own)?;
    std::fs::create_dir_all(&sibling)?;
    std::fs::write(own.join("input.rs"), "alice")?;
    std::fs::write(sibling.join("input.rs"), "active bob build")?;
    let result = command(
        &f.app,
        "alice",
        "cache_evict",
        json!({"max_bytes":0,"max_age_secs":0,"max_entries":100}),
    )
    .await?;
    assert_eq!(result["ok"], true, "{result}");
    assert!(!own.exists(), "eviction did not exercise its deletion path");
    assert_eq!(
        std::fs::read_to_string(sibling.join("input.rs"))?,
        "active bob build"
    );
    Ok(())
}

#[tokio::test]
async fn tenant_admin_cannot_create_machines_or_launch_arbitrary_host_processes() -> Result<()> {
    let f = fixture().await?;
    for payload in [
        json!({"command":"machine.create","args":{"root":"/"}}),
        json!({"command":"process.start","args":{"machine":"local","program":"/bin/sh","args":["-c","true"]}}),
        json!({"command":"model.state","args":{}}),
    ] {
        let result = request(&f.app, "alice", "POST", "/v1/command", payload).await?;
        assert_eq!(result.status(), StatusCode::FORBIDDEN);
    }
    let runtime = &f.alice.service.runtime;
    assert!(runtime.create_machine(std::path::Path::new("/")).is_err());
    for op in ["exec", "fs.read", "llm"] {
        let result = runtime
            .perform(json!({"op":op,"args":{}}), &format!("deny-{op}"), 0)
            .await;
        assert!(
            result.unwrap_err().to_string().contains("host authority"),
            "{op}"
        );
    }
    assert!(
        runtime
            .perform(json!({"op":"now","args":null}), "allowed-clock", 0)
            .await?
            .is_number()
    );
    assert!(
        Authorizer::new(vec![TokenConfig {
            token: "bad".into(),
            tenant: TenantId::new("alice")?,
            scopes: BTreeSet::from([Scope::Host])
        }])
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn host_filesystem_replay_is_denied_to_restricted_runtime_clone() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("secret"), "host-only")?;
    let service = Service::new(
        Store::memory()?,
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )?;
    let machine = service.runtime.create_machine(directory.path())?;
    let descriptor = json!({"op":"fs.read","args":{"machine":machine.id,"path":"secret"}});
    assert_eq!(
        service
            .runtime
            .perform(descriptor.clone(), "host-read", 0)
            .await?,
        "host-only"
    );
    let restricted = service.runtime.without_host_authority();
    let result = restricted.perform(descriptor, "host-read", 0).await;
    assert!(result.unwrap_err().to_string().contains("host authority"));
    // Restricting one request must not revoke the daemon's trusted owner clone.
    assert_eq!(
        service
            .runtime
            .perform(
                json!({"op":"fs.read","args":{"machine":machine.id,"path":"secret"}}),
                "second-host-read",
                0
            )
            .await?,
        "host-only"
    );
    Ok(())
}

#[test]
fn cloned_services_cannot_be_relabelled_as_isolated_tenants() -> Result<()> {
    let base = Service::new(
        Store::memory()?,
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )?;
    let alice = Arc::new(base.clone().with_tenant(TenantId::new("alice")?));
    let bob = Arc::new(base.with_tenant(TenantId::new("bob")?));
    let error = ServiceDirectory::new([alice, bob])
        .err()
        .context("aliased services admitted")?;
    assert!(error.to_string().contains("separate storage and runtimes"));
    Ok(())
}

#[test]
fn separately_opened_database_handles_cannot_alias_tenants() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("shared.sqlite");
    let mut services = Vec::new();
    for name in ["alice", "bob"] {
        services.push(Arc::new(
            Service::new(
                Store::open(&database)?,
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
                vec![Lang::Rust],
            )?
            .with_tenant(TenantId::new(name)?),
        ));
    }
    assert!(ServiceDirectory::new(services).is_err());
    Ok(())
}
