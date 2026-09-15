use anyhow::{Context, Result};
use loom_api::Service;
use loom_proto::{CommandRequest, Lang};
use loom_store::Store;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

fn service(store: Store) -> Result<Service> {
    Ok(Service::new(
        store,
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::TypeScript, Lang::JavaScript, Lang::Rust],
    )?
    .with_driver_path(PathBuf::from("/nonexistent/typescript-must-not-run-rustc")))
}

async fn command(service: &Service, name: &str, args: Value) -> loom_proto::Response {
    service
        .command(CommandRequest {
            session: None,
            command: name.into(),
            args,
        })
        .await
}

#[tokio::test]
async fn default_typescript_transforms_enum_and_parameter_properties_and_reopens() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("store.sqlite");
    let app = service(Store::open(&path)?)?;
    let source = r#"
        enum Operation { Add = 1, Multiply = 2 }
        class Calculation {
            constructor(public value: number) {}
            apply(other: number, operation: Operation): number {
                return operation === Operation.Add ? this.value + other : this.value * other;
            }
        }
        function main(value: number, other: number): number {
            return new Calculation(value).apply(other, Operation.Add);
        }
    "#;
    let added = command(&app, "add", json!({"name":"sum","source":source})).await;
    assert!(added.ok, "{added:?}");
    let definition = app.store.resolve("sum")?.context("definition missing")?;
    assert_eq!(definition.lang, Lang::TypeScript);
    assert_eq!(app.store.source(&definition.hash)?.as_deref(), Some(source));
    let run = command(&app, "run", json!({"target":"sum","args":[20,22]})).await;
    assert!(run.ok, "{run:?}");
    assert_eq!(run.result["output"], 42);
    drop(app);
    let app = service(Store::open(&path)?)?;
    assert_eq!(app.store.resolve("sum")?.unwrap().hash, definition.hash);
    let run = command(&app, "run", json!({"target":"sum","args":[1,2]})).await;
    assert!(run.ok, "{run:?}");
    assert_eq!(run.result["output"], 3);
    Ok(())
}

#[tokio::test]
async fn plain_javascript_is_valid_default_typescript() -> Result<()> {
    let app = service(Store::memory()?)?;
    let source = "const main = (value) => ({value, ambient: typeof Deno, node: typeof process});";
    let added = command(&app, "add", json!({"name":"plain","source":source})).await;
    assert!(added.ok, "{added:?}");
    let run = command(&app, "run", json!({"target":"plain","args":[42]})).await;
    assert!(run.ok, "{run:?}");
    assert_eq!(
        run.result["output"],
        json!({"value":42,"ambient":"undefined","node":"undefined"})
    );
    Ok(())
}

#[tokio::test]
async fn malformed_typescript_does_not_publish() -> Result<()> {
    let app = service(Store::memory()?)?;
    let added = command(
        &app,
        "add",
        json!({"name":"invalid","source":"function main(value: ): number { return value; }"}),
    )
    .await;
    assert!(!added.ok, "{added:?}");
    assert!(
        format!("{added:?}").to_lowercase().contains("typescript"),
        "{added:?}"
    );
    assert!(app.store.resolve("invalid")?.is_none());
    Ok(())
}

#[tokio::test]
async fn typescript_actor_uses_json_messages_and_durable_sql() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let app = service(Store::memory()?)?;
    let node = loom_actor::Node::new(
        directory.path().join("actors"),
        app.actor_registry(),
        Arc::new(loom_actor::DefaultEffects),
        loom_actor::Config::default(),
    )
    .await?;
    let app = app.with_actors(node.clone());
    let source = r#"
        type Message = { value: number };
        const LOOM_SCHEMA = "CREATE TABLE arrivals(value INTEGER)";
        const main = loom.messages.json(async (message: Message): Promise<void> => {
            await loom.sql("INSERT INTO arrivals VALUES (?)", [message.value]);
        });
    "#;
    let added = command(&app, "add", json!({"name":"typed","source":source})).await;
    assert!(added.ok, "{added:?}");
    let spawned = command(&app, "spawn", json!({"def":"typed","init":{"value":42}})).await;
    assert!(spawned.ok, "{spawned:?}");
    let id = spawned.result["id"].as_str().context("actor ID missing")?;
    node.run_until_idle().await?;
    let rows = node
        .open(id)
        .await?
        .inspect_sql("SELECT value FROM arrivals", Vec::new())
        .await?;
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.rows[0].get::<i64>(0)?, 42);
    node.close().await?;
    Ok(())
}

#[tokio::test]
async fn typescript_identity_checks_compiler_policy_and_source() -> Result<()> {
    let app = service(Store::memory()?)?;
    let source = "function main(): number { return 42; }";
    let added = command(
        &app,
        "add",
        json!({"name":"typed","source":source,"allowed_effects":[]}),
    )
    .await;
    assert!(added.ok, "{added:?}");
    let definition = app.store.resolve("typed")?.context("definition missing")?;
    let abi = loom_v8::typescript_abi();
    assert_eq!(app.store.javascript_source(&definition.hash, &abi)?, source);
    assert!(
        app.store
            .javascript_source(&definition.hash, loom_v8::ABI_VERSION)
            .is_err()
    );
    app.store.with_connection(|connection| {
        connection.execute(
            "UPDATE defs SET allowed_effects=NULL WHERE hash=?",
            [&definition.hash],
        )?;
        Ok(())
    })?;
    assert!(app.store.javascript_source(&definition.hash, &abi).is_err());
    Ok(())
}
