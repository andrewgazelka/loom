mod support;
use anyhow::{Context, Result};
use loom_proto::Lang;
use loom_rt::Runtime;
use loom_store::Store;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let first = args.next().context("first component path required")?;
    let second = args.next().context("upgraded component path required")?;
    let database = args.next().context("database path required")?;
    let store = Store::open(&database)?;
    let mut hashes = Vec::new();
    for path in [first, second] {
        let hash = support::register(&store, Lang::Rust, path, "recovery fixture")?;
        hashes.push(hash);
    }
    let runtime = Runtime::new(store.clone())?;
    let actor = runtime.spawn(&hashes[0], json!(0)).await?;
    let events = vec![json!(1); 10_000];
    store.append_batch(&actor.id, &events, 1)?;
    assert_eq!(runtime.state(&actor.id).await?, json!(10_000));
    assert!(store.latest_snapshot(&actor.id, &hashes[0])?.is_some());
    drop(runtime);
    drop(store);
    let store = Store::open(&database)?;
    let runtime = Runtime::new(store.clone())?;
    assert_eq!(runtime.state(&actor.id).await?, json!(10_000));
    assert_eq!(runtime.upgrade(&actor.id, &hashes[1]).await?, json!(20_000));
    let fork = runtime.fork_actor(&actor.id).await?;
    assert_eq!(runtime.send(&fork.id, json!(2)).await?, json!(20_004));
    assert_eq!(runtime.state(&actor.id).await?, json!(20_000));
    assert_eq!(runtime.upgrade(&fork.id, &hashes[0]).await?, json!(10_002));
    println!("M4: 10000 events recovered; fold upgrade refolded from 0; fork isolated");
    Ok(())
}
