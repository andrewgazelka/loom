mod support;
use anyhow::{Context, Result};
use loom_proto::Lang;
use loom_rt::Runtime;
use loom_store::Store;
use serde_json::json;
#[tokio::main]
async fn main() -> Result<()> {
    let paths = std::env::args().skip(1).collect::<Vec<_>>();
    anyhow::ensure!(
        !paths.is_empty(),
        "provide real TS component paths (each is a distinct definition)"
    );
    let store = Store::memory()?;
    let runtime = Runtime::new(store.clone())?;
    for path in paths {
        let hash = support::register(&store, Lang::Ts, &path, &path)?;
        for iteration in 0..2 {
            let start = std::time::Instant::now();
            let call = runtime
                .call_def_timed(&hash, json!([12, 30]))
                .await
                .with_context(|| format!("component {path}"))?;
            println!(
                "{}",
                json!({"path":path,"iteration":iteration,"total_ms":start.elapsed().as_secs_f64()*1000.0,"call":call})
            );
        }
    }
    Ok(())
}
