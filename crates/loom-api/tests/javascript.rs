use anyhow::{Context, Result};
use loom_api::Service;
use loom_proto::{CommandRequest, Lang};
use loom_store::Store;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf};

fn service(store: Store) -> Result<Service> {
    // A missing Rust driver proves JavaScript admission never invokes rustc.
    Ok(Service::new(
        store,
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust, Lang::JavaScript],
    )?
    .with_driver_path(PathBuf::from("/nonexistent/loom-javascript-rustc")))
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

fn counts(store: &Store) -> Result<BTreeMap<String, i64>> {
    store.with_connection(|connection| {
        let names = connection
            .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        names
            .into_iter()
            .map(|name| {
                let sql = format!("SELECT count(*) FROM \"{}\"", name.replace('"', "\"\""));
                Ok((name, connection.query_row(&sql, [], |row| row.get(0))?))
            })
            .collect()
    })
}

#[tokio::test]
async fn javascript_add_runs_and_reopens_without_rust_compiler() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("store.sqlite");
    let app = service(Store::open(&path)?)?;
    let added = command(&app, "add", json!({"name":"sum", "lang":"javascript", "source":"async function main(a, b) { return a + b; }"})).await;
    assert!(added.ok, "{added:?}");
    let hash = app
        .store
        .resolve("sum")?
        .context("definition not published")?
        .hash;
    let result = command(&app, "run", json!({"target":"sum", "args":[20,22]})).await;
    assert!(result.ok, "{result:?}");
    assert_eq!(result.result["output"], 42);
    drop(app);
    let reopened = service(Store::open(&path)?)?;
    assert_eq!(
        reopened
            .store
            .resolve("sum")?
            .context("definition lost on reopen")?
            .hash,
        hash
    );
    let result = command(&reopened, "run", json!({"target":"sum", "args":[1,2]})).await;
    assert!(result.ok, "{result:?}");
    assert_eq!(result.result["output"], 3);
    Ok(())
}

#[tokio::test]
async fn invalid_javascript_preserves_live_store() -> Result<()> {
    let app = service(Store::memory()?)?;
    for source in [
        "async function main( {",
        "const main = 42;",
        "import x from 'missing'; async function main() {}",
    ] {
        let before = counts(&app.store)?;
        let response = command(
            &app,
            "add",
            json!({"name":"invalid", "lang":"javascript", "source":source}),
        )
        .await;
        assert!(!response.ok, "invalid source accepted: {response:?}");
        assert_eq!(
            counts(&app.store)?,
            before,
            "failed admission changed live store"
        );
    }
    Ok(())
}

#[tokio::test]
async fn javascript_rejects_dependencies_and_unknown_language() -> Result<()> {
    let app = service(Store::memory()?)?;
    for args in [
        json!({"name":"dependency", "lang":"javascript", "source":"async function main() {}", "deps":{"x":"missing"}}),
        json!({"name":"unknown", "lang":"typescript", "source":"async function main() {}"}),
    ] {
        let before = counts(&app.store)?;
        let response = command(&app, "add", args).await;
        assert!(!response.ok, "{response:?}");
        assert_eq!(counts(&app.store)?, before);
    }
    Ok(())
}

#[tokio::test]
async fn javascript_update_preserves_language_and_changes_identity() -> Result<()> {
    let app = service(Store::memory()?)?;
    let added = command(&app, "add", json!({"name":"value", "lang":"javascript", "source":"async function main() { return 1; }"})).await;
    assert!(added.ok, "{added:?}");
    let original = app.store.resolve("value")?.context("definition missing")?;
    let updated = command(
        &app,
        "update",
        json!({"name":"value", "source":"async function main() { return 2; }"}),
    )
    .await;
    assert!(updated.ok, "{updated:?}");
    let current = app
        .store
        .resolve("value")?
        .context("updated definition missing")?;
    assert_eq!(current.lang, Lang::JavaScript);
    assert_ne!(current.hash, original.hash);
    let result = command(&app, "run", json!({"target":"value", "args":[]})).await;
    assert!(result.ok, "{result:?}");
    assert_eq!(result.result["output"], 2);
    Ok(())
}

#[tokio::test]
async fn javascript_admitted_after_node_start_executes_durable_actor_sql() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let app = service(Store::memory()?)?;
    let node = loom_actor::Node::new(
        directory.path().join("actors"),
        app.actor_registry(),
        std::sync::Arc::new(loom_actor::DefaultEffects),
        loom_actor::Config::default(),
    )
    .await?;
    let app = app.with_actors(node.clone());
    let source = r#"
const LOOM_SCHEMA = "CREATE TABLE arrivals(value INTEGER)";
async function main(message) {
    await loom.perform("sql", {sql: "INSERT INTO arrivals VALUES (42)", params: []});
}
"#;
    let added = command(
        &app,
        "add",
        json!({"name":"arrival", "lang":"javascript", "source":source}),
    )
    .await;
    assert!(added.ok, "{added:?}");
    let spawned = command(&app, "spawn", json!({"def":"arrival", "init":{}})).await;
    assert!(spawned.ok, "{spawned:?}");
    let id = spawned.result["id"]
        .as_str()
        .context("spawned actor ID missing")?;
    node.run_until_idle().await?;
    let info = node.info(id).await?;
    assert_eq!(
        info.status,
        loom_actor::Status::Running,
        "actor failed: {}",
        info.reason
    );
    let actor = node.open(id).await?;
    let rows = actor
        .inspect_sql("SELECT value FROM arrivals", Vec::new())
        .await?;
    assert_eq!(
        rows.rows.len(),
        1,
        "actor cursor={} inbox={} reason={}",
        info.cursor,
        info.inbox_len,
        info.reason
    );
    assert_eq!(rows.rows[0].get::<i64>(0)?, 42);
    assert_eq!(
        node.info(id).await?.behavior_hash,
        added.result["hash"]
            .as_str()
            .context("admitted hash missing")?
    );
    Ok(())
}

#[tokio::test]
async fn javascript_dynamic_effects_are_traced_and_policy_cannot_be_caught() -> Result<()> {
    let app = service(Store::memory()?)?;
    let source = r#"async function main() {
        try { return await loom.perform(["n", "ow"].join(""), null); }
        catch (_) { return "swallowed"; }
    }"#;
    for allowed in [true, false] {
        let name = if allowed { "clock" } else { "denied-clock" };
        let effects = if allowed { json!(["now"]) } else { json!([]) };
        let added = command(
            &app,
            "add",
            json!({"name":name, "lang":"javascript", "source":source, "allowed_effects":effects}),
        )
        .await;
        assert!(added.ok, "{added:?}");
        let called = command(&app, "run", json!({"target":name, "args":[]})).await;
        if allowed {
            assert!(called.ok, "{called:?}");
            assert!(called.result["output"].is_number(), "{called:?}");
            let effects = called.result["effects"]
                .as_array()
                .context("trace effects missing")?;
            assert_eq!(effects.len(), 1);
            assert_eq!(effects[0]["descriptor"]["op"], "now");
        } else {
            assert!(!called.ok, "guest swallowed denied effect: {called:?}");
            assert!(format!("{called:?}").contains("not allowed"), "{called:?}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn javascript_json_actor_handles_spawn_init_and_send_with_plain_sql_values() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let app = service(Store::memory()?)?;
    let node = loom_actor::Node::new(
        directory.path().join("actors"),
        app.actor_registry(),
        std::sync::Arc::new(loom_actor::DefaultEffects),
        loom_actor::Config::default(),
    )
    .await?;
    let app = app.with_actors(node.clone());
    let source = r#"
const LOOM_SCHEMA = "CREATE TABLE arrivals(amount INTEGER, body TEXT)";
const main = loom.messages.json(async message => {
    await loom.sql("INSERT INTO arrivals VALUES (?, ?)", [message.amount, message.body]);
    const rows = await loom.sql("SELECT amount, body FROM arrivals ORDER BY rowid DESC LIMIT 1");
    if (rows.length !== 1 || rows[0].amount !== message.amount || rows[0].body !== message.body) {
        throw new Error("SQL row objects did not preserve the message");
    }
});
"#;
    let added = command(
        &app,
        "add",
        json!({"name":"json-arrival", "lang":"javascript", "source":source}),
    )
    .await;
    assert!(added.ok, "{added:?}");
    let spawned = command(
        &app,
        "spawn",
        json!({"def":"json-arrival", "init":{"amount":7,"body":"λ snow 雪"}}),
    )
    .await;
    assert!(spawned.ok, "{spawned:?}");
    let id = spawned.result["id"]
        .as_str()
        .context("spawned actor ID missing")?;
    node.run_until_idle().await?;
    let sent = command(
        &app,
        "send",
        json!({"id":id, "msg":{"amount":35,"body":"hello 👋"}}),
    )
    .await;
    assert!(sent.ok, "{sent:?}");
    let actor = node.open(id).await?;
    let rows = actor
        .inspect_sql(
            "SELECT amount, body FROM arrivals ORDER BY rowid",
            Vec::new(),
        )
        .await?;
    let info = node.info(id).await?;
    assert_eq!(info.status, loom_actor::Status::Running, "{}", info.reason);
    assert_eq!(
        rows.rows.len(),
        2,
        "cursor={} inbox={} reason={}",
        info.cursor,
        info.inbox_len,
        info.reason
    );
    assert_eq!(rows.rows[0].get::<i64>(0)?, 7);
    assert_eq!(rows.rows[0].get::<String>(1)?, "λ snow 雪");
    assert_eq!(rows.rows[1].get::<i64>(0)?, 35);
    assert_eq!(rows.rows[1].get::<String>(1)?, "hello 👋");
    Ok(())
}
