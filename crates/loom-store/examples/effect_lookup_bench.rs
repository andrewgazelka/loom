use loom_store::Store;
use serde_json::json;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let store = Store::memory()?;
    let mut recorded = 0;
    for events in [0, 512, 2048] {
        while recorded < events {
            store.effect_put("scan", "observed", recorded, &json!(recorded))?;
            recorded += 1;
        }
        let started = Instant::now();
        for occurrence in 0..100 {
            assert!(store.effect_get("fresh", "new-call", occurrence)?.is_none());
        }
        println!(
            "{}",
            json!({"events": events, "misses": 100, "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0})
        );
    }
    Ok(())
}
