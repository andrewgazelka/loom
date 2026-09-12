//! Profile the real checker, component builder, and runtime without the user's database.
use anyhow::{Context, Result, ensure};
use loom_api::Service;
use loom_proto::{CommandRequest, Lang};
use loom_store::Store;
use serde_json::json;
use std::{path::PathBuf, process::ExitCode, time::Instant};
fn main() -> Result<ExitCode> {
    if let Some(status) = loom_build::compiler_cache_entry()? {
        return Ok(status);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())?;
    Ok(ExitCode::SUCCESS)
}

async fn run() -> Result<()> {
    let root = PathBuf::from(std::env::var("LOOM_ROOT")?).canonicalize()?;
    let service = Service::new(Store::memory()?, root, vec![Lang::Rust])?;
    for expression in ["2 + 2", "2 + 3", "42 * 2"] {
        let start = Instant::now();
        let definition = service
            .command(CommandRequest {
                session: None, command: "add".into(),
                args: json!({"name":"timing/expression", "source":format!("pub fn main() -> i32 {{ {expression} }}")}),
            })
            .await;
        ensure!(definition.ok, "{definition:?}");
        let define_ms = start.elapsed().as_secs_f64() * 1000.0;
        let hash = definition.result["def"]["hash"]
            .as_str()
            .context("definition hash")?;
        for iteration in 0..2 {
            let result = service.runtime.call_def_timed(hash, json!([])).await?;
            println!(
                "{}",
                json!({"operation":"run","expression":expression,"iteration":iteration,"define_ms":define_ms,"result":result})
            );
        }
    }
    Ok(())
}
