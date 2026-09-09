use anyhow::{Context, Result, ensure};
use loom_rt::Runtime;
use loom_store::Store;
use serde_json::json;
#[tokio::main]
async fn main() -> Result<()> {
    let count = std::env::args()
        .nth(1)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(10_000);
    let root = tempfile::tempdir()?;
    for index in 0..count {
        std::fs::File::create(root.path().join(format!("file{index:06}")))?
            .set_len((index % 1024) as u64)?;
    }
    let runtime = Runtime::new(Store::memory()?)?;
    let machine = runtime.create_machine(root.path())?;
    for iteration in 0..3 {
        let start = std::time::Instant::now();
        let listing = runtime
            .perform(
                json!({"op":"fs.list","args":{"machine":machine.id,"path":"/"}}),
                &format!("list-benchmark:{iteration}"),
                0,
            )
            .await?;
        let entries = listing.as_array().context("listing not an array")?;
        ensure!(entries.len() == count, "listing count mismatch");
        ensure!(
            entries.iter().all(|entry| entry["is_dir"] == false),
            "file classified as directory"
        );
        println!(
            "{}",
            json!({"iteration":iteration,"entries":entries.len(),"total_ms":start.elapsed().as_secs_f64()*1000.0})
        );
    }
    Ok(())
}
