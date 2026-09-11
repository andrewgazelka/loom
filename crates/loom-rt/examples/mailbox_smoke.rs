mod support;
use anyhow::{Context, Result, ensure};
use loom_proto::Lang;
use loom_rt::Runtime;
use loom_store::Store;
use serde_json::json;
#[tokio::main]
async fn main() -> Result<()> {
    let component = std::env::args().nth(1).context("component required")?;
    let store = Store::memory()?;
    let hash = support::register(&store, Lang::Rust, component, "mailbox fixture")?;
    let runtime = Runtime::new(store.clone())?;
    let actor = runtime.spawn(&hash, json!(0)).await?;
    runtime
        .send(&actor.id, json!({"actor":actor.id,"remaining":3}))
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if runtime.state(&actor.id).await? == json!(4) && store.pending_messages()?.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    ensure!(
        runtime.state(&actor.id).await? == json!(4),
        "self-send state mismatch"
    );
    let descriptor =
        json!({"op":"actor.send","args":{"actor":actor.id,"msg":{"actor":actor.id,"remaining":0}}});
    runtime.perform(descriptor.clone(), "replay", 0).await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while runtime.state(&actor.id).await? != json!(5) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    runtime.perform(descriptor, "replay", 0).await?;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    ensure!(
        runtime.state(&actor.id).await? == json!(5),
        "replayed actor.send duplicated delivery"
    );
    let count = store.actors()?.len();
    let descriptor = json!({"op":"actor.spawn","args":{"def":hash,"state":0}});
    let first = runtime
        .perform(descriptor.clone(), "actor.spawn-replay", 0)
        .await?;
    let repeated = runtime.perform(descriptor, "actor.spawn-replay", 0).await?;
    ensure!(first == repeated, "actor.spawn receipt changed on replay");
    ensure!(
        store.actors()?.len() == count + 1,
        "actor.spawn replay created another actor"
    );
    println!("mailbox self-send and idempotent completed-delivery replay pass");
    Ok(())
}
