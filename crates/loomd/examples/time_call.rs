//! Attribute a compiled call to runtime execution and the real reply barrier.
//! Use an isolated database: every measured call records its actual effects.
use anyhow::{Context, Result, ensure};
use clap::Parser;
use loom_proto::Lang;
use std::{path::PathBuf, time::Instant};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    db: PathBuf,
    #[arg(long)]
    root: PathBuf,
    #[arg(long)]
    fixture: PathBuf,
    #[arg(long)]
    hash: String,
    #[arg(long, default_value_t = 7)]
    rounds: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!((1..=1000).contains(&args.rounds), "rounds must be 1..1000");
    let service = loom_api::Service::new(
        loom_store::Store::open(&args.db)?,
        args.root.canonicalize()?,
        vec![Lang::Rust],
    )?;
    let definition = service
        .store
        .executable_definition(&args.hash)?
        .context("definition missing from diagnostic database")?;
    let component = definition
        .component_hash
        .context("definition must already be compiled")?;
    ensure!(
        service.store.get(&component)?.is_some(),
        "compiled component is missing"
    );
    let machine = service.runtime.create_machine(&args.fixture)?;
    service.store.flush()?;
    for round in 0..=args.rounds {
        let recording_before = service.store.recording_timings();
        let start = Instant::now();
        let called = service
            .runtime
            .call_def_timed(&args.hash, serde_json::json!([machine.id, "."]))
            .await?;
        let call_ms = start.elapsed().as_secs_f64() * 1000.0;
        let reply_start = Instant::now();
        let reply = service.response(Ok(called.value));
        let reply_ms = reply_start.elapsed().as_secs_f64() * 1000.0;
        let recording_after = service.store.recording_timings();
        ensure!(reply.ok, "reply failed: {}", reply.result);
        println!(
            "{}",
            serde_json::json!({
                "round":round, "warmup":round == 0, "call_ms":call_ms,
                "reply_ms":reply_ms, "total_ms":start.elapsed().as_secs_f64() * 1000.0,
                "runtime":called.timing, "result":reply.result,
                "recording_commits":recording_after.committed_transactions - recording_before.committed_transactions,
                "transaction_ms":(recording_after.transaction_nanos - recording_before.transaction_nanos) as f64 / 1_000_000.0,
                "checkpoint_ms":(recording_after.checkpoint_nanos - recording_before.checkpoint_nanos) as f64 / 1_000_000.0,
                "checkpoint_attempts":recording_after.checkpoint_attempts - recording_before.checkpoint_attempts,
            })
        );
    }
    Ok(())
}
