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
        .context("usage: component_smoke CORE_WASM [recursive|itoa]")?;
    let lang = Lang::Rust;
    let mode = args.next().unwrap_or_default();
    let store = Store::memory()?;
    let hash = support::register(&store, lang, path, "smoke")?;
    let runtime = Runtime::new(store.clone())?;
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
    Ok(())
}
