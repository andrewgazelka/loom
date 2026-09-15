//! Measures the real fresh-isolate/cache path without a machine-specific gate.
use anyhow::{Result, ensure};
use loom_sandbox::{CallEffects, Sandbox};
use loom_v8::{Limits, V8Engine};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, time::Instant};

struct NoEffects;

impl CallEffects for NoEffects {
    fn perform(
        &mut self,
        descriptor: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + '_>> {
        Box::pin(async move { anyhow::bail!("benchmark unexpectedly requested {descriptor}") })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let calls = arguments
        .next()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(500);
    ensure!(arguments.next().is_none(), "usage: benchmark [calls]");
    ensure!(
        (1..=10_000).contains(&calls),
        "calls must be between 1 and 10000"
    );
    let engine = V8Engine::new(Limits::default())?;
    let compilation = Instant::now();
    let sandbox = engine
        .compile("function main(value) { return value + 1; }")
        .await?;
    let compile_ms = compilation.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let mut samples_us = Vec::with_capacity(calls);
    for value in 0..calls {
        let call = Instant::now();
        let output = sandbox.call(json!([value]), &mut NoEffects).await?;
        samples_us.push(call.elapsed().as_secs_f64() * 1_000_000.0);
        ensure!(output == json!(value + 1), "incorrect benchmark result");
    }
    let total_ms = started.elapsed().as_secs_f64() * 1000.0;
    samples_us.sort_by(f64::total_cmp);
    let stats = engine.cache_stats();
    ensure!(
        stats.compilations == 1 && stats.cache_hits == calls as u64,
        "benchmark did not consume its compiled cache on every invocation"
    );
    println!(
        "{}",
        json!({
            "engine": "v8",
            "calls": calls,
            "compile_ms": compile_ms,
            "warm_p50_us": samples_us[calls / 2],
            "warm_p99_us": samples_us[(calls * 99 / 100).min(calls - 1)],
            "total_ms": total_ms,
            "cache_hits": stats.cache_hits,
            "compilations": stats.compilations,
        })
    );
    Ok(())
}
