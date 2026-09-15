use anyhow::{Context, Result};
use loom_actor::{DefaultEffects, Node};
use loom_api::Service;
use loom_behavior::StoreEffects;
use loom_proto::{CommandRequest, Lang};
use loom_store::Store;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

fn service(store: Store) -> Result<Service> {
    Service::new(
        store,
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::JavaScript],
    )
}
async fn command(app: &Service, name: &str, args: Value) -> loom_proto::Response {
    app.command(CommandRequest {
        session: None,
        command: name.into(),
        args,
    })
    .await
}
async fn node(app: &Service, path: &Path) -> Result<Node> {
    Node::new(
        path,
        app.actor_registry(),
        Arc::new(StoreEffects::new(
            app.store.clone(),
            Arc::new(DefaultEffects),
        )),
        Default::default(),
    )
    .await
}

#[tokio::test]
async fn guest_cas_roundtrip_reopens_and_other_tenant_cannot_read() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("store.sqlite");
    let source = "async function main(bytes) { const ref = await loom.cas.put(bytes); return {ref, bytes: await loom.cas.get(ref)}; }";
    let app = service(Store::open(&database)?)?;
    let added = command(
        &app,
        "add",
        json!({"name":"cas","lang":"javascript","source":source}),
    )
    .await;
    assert!(added.ok, "{added:?}");
    let run = command(&app, "run", json!({"target":"cas","args":[[0,127,255]]})).await;
    assert!(run.ok, "{run:?}");
    assert_eq!(run.result["output"]["bytes"], json!([0, 127, 255]));
    let reference = run.result["output"]["ref"].clone();
    drop(app);
    let reopened = service(Store::open(&database)?)?;
    let reader = "async function main(ref) { return await loom.cas.get(ref) }";
    let added = command(
        &reopened,
        "add",
        json!({"name":"reader","lang":"javascript","source":reader}),
    )
    .await;
    assert!(added.ok, "{added:?}");
    let read = command(
        &reopened,
        "run",
        json!({"target":"reader","args":[reference]}),
    )
    .await;
    assert!(read.ok, "{read:?}");
    assert_eq!(read.result["output"], json!([0, 127, 255]));
    let other = service(Store::memory()?)?;
    let added = command(
        &other,
        "add",
        json!({"name":"reader","lang":"javascript","source":reader}),
    )
    .await;
    assert!(added.ok, "{added:?}");
    let denied = command(&other, "run", json!({"target":"reader","args":[reference]})).await;
    assert!(!denied.ok, "{denied:?}");
    Ok(())
}

#[tokio::test]
async fn actors_pass_cas_refs_with_recorded_effects_and_reopen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("store.sqlite");
    let source = r#"
const LOOM_SCHEMA = 'CREATE TABLE received(reference TEXT, body TEXT)';
const main = loom.messages.json(async message => {
  if (message.type === 'send') {
    const ref = await loom.cas.put(message.bytes);
    await (await loom.actors.named('receiver')).send({type:'read', ref});
  } else if (message.type === 'read') {
    const bytes = await loom.cas.get(message.ref);
    await loom.sql('INSERT INTO received VALUES (?, ?)', [message.ref.$ref, JSON.stringify(bytes)]);
  }
});
"#;
    let app = service(Store::open(&database)?)?;
    let added = command(
        &app,
        "add",
        json!({"name":"casActor","lang":"javascript","source":source}),
    )
    .await;
    assert!(added.ok, "{added:?}");
    let hash = app
        .store
        .resolve("casActor")?
        .context("actor definition missing")?
        .hash;
    let actors = directory.path().join("actors");
    let host = node(&app, &actors).await?;
    let sender = host.spawn_root(&hash, b"{}").await?;
    let receiver = host.spawn_root(&hash, b"{}").await?;
    host.register("receiver", &receiver).await?;
    host.send(
        &sender,
        "send-reference",
        &serde_json::to_vec(&json!({"type":"send","bytes":[3,4,255]}))?,
    )
    .await?;
    host.run_until_idle().await?;
    let actor = host.open(&receiver).await?;
    let rows = actor
        .inspect_sql("SELECT reference, body FROM received", Vec::new())
        .await?;
    assert_eq!(rows.rows.len(), 1, "{:?}", host.info(&receiver).await?);
    assert_eq!(rows.rows[0].get::<String>(1)?, "[3,4,255]");
    let reference = json!({"$ref":rows.rows[0].get::<String>(0)?});
    assert!(matches!(
        host.validate(&receiver, &hash, 1).await?,
        loom_actor::Verdict::Matched { .. }
    ));
    drop(actor);
    host.close().await?;
    drop(host);
    drop(app);
    let app = service(Store::open(&database)?)?;
    let host = node(&app, &actors).await?;
    host.send(
        &receiver,
        "reread-reference",
        &serde_json::to_vec(&json!({"type":"read","ref":reference}))?,
    )
    .await?;
    host.run_until_idle().await?;
    let actor = host.open(&receiver).await?;
    assert_eq!(
        actor
            .inspect_sql("SELECT body FROM received", Vec::new())
            .await?
            .rows
            .len(),
        2
    );
    drop(actor);
    host.close().await?;
    Ok(())
}

#[tokio::test]
async fn guest_cas_byte_boundaries_and_codec_errors_use_real_v8_path() -> Result<()> {
    let app = service(Store::memory()?)?;
    let source = r#"async function main(count) {
      const reference = await loom.cas.put(Array(count).fill(255));
      return (await loom.cas.get(reference)).length;
    }"#;
    let added = command(
        &app,
        "add",
        json!({"name":"bytes","lang":"javascript","source":source}),
    )
    .await;
    assert!(added.ok, "{added:?}");
    let maximal = command(
        &app,
        "run",
        json!({"target":"bytes","args":[loom_store::CAS_GUEST_MAX_BYTES]}),
    )
    .await;
    assert!(maximal.ok, "{maximal:?}");
    assert_eq!(maximal.result["output"], loom_store::CAS_GUEST_MAX_BYTES);
    let oversized = command(
        &app,
        "run",
        json!({"target":"bytes","args":[loom_store::CAS_GUEST_MAX_BYTES+1]}),
    )
    .await;
    assert!(!oversized.ok, "{oversized:?}");
    let source =
        "async function main(){return await loom.cas.get(await loom.cas.putJson({value:42}))}";
    let added = command(
        &app,
        "add",
        json!({"name":"codec","lang":"javascript","source":source}),
    )
    .await;
    assert!(added.ok, "{added:?}");
    let invalid = command(&app, "run", json!({"target":"codec","args":[]})).await;
    assert!(!invalid.ok, "{invalid:?}");
    Ok(())
}
