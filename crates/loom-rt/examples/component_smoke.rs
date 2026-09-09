mod support;
use anyhow::{Context, Result};
use loom_proto::Lang;
use loom_rt::Runtime;
use loom_store::Store;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .context("usage: component_smoke COMPONENT [ts|rust] [counter|recursive|itoa]")?;
    let lang = if args.next().as_deref() == Some("ts") {
        Lang::Ts
    } else {
        Lang::Rust
    };
    let mode = args.next().unwrap_or_default();
    let counter = mode == "counter";
    let store = Store::memory()?;
    let hash = support::register(&store, lang, path, "smoke")?;
    let runtime = Runtime::new(store.clone())?;
    if counter {
        let actor = runtime.spawn(&hash, json!(null)).await?;
        assert_eq!(runtime.send(&actor.id, json!(7)).await?, json!(7));
        drop(runtime);
        let restarted = Runtime::new(store)?;
        assert_eq!(restarted.state(&actor.id).await?, json!(7));
        assert_eq!(restarted.send(&actor.id, json!(2)).await?, json!(9));
        let fork = restarted.fork_actor(&actor.id).await?;
        assert_eq!(restarted.state(&fork.id).await?, json!(9));
        println!("counter restart and fork pass");
    } else {
        let value = runtime
            .call_def(
                &hash,
                match mode.as_str() {
                    "recursive" => json!([3]),
                    "itoa" => json!([42]),
                    _ => json!([12, 30]),
                },
            )
            .await?;
        assert_eq!(
            value,
            match mode.as_str() {
                "recursive" => json!(3),
                "itoa" => json!("42"),
                _ => json!(42),
            }
        );
        println!("call pass: {value}");
    }
    Ok(())
}
