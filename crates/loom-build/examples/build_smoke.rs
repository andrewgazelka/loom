use loom_build::Builder;
use loom_check::Checker;
use loom_proto::{DefineRequest, Lang};
use std::{collections::BTreeMap, path::PathBuf};

fn main() -> Result<std::process::ExitCode, Box<dyn std::error::Error>> {
    if let Some(status) = loom_build::compiler_cache_entry()? {
        return Ok(status);
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run())?;
    Ok(std::process::ExitCode::SUCCESS)
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().ok_or("missing repository root")?);
    let source_path = args.next().ok_or("missing source path")?;
    let output_path = args.next().ok_or("missing output path")?;
    let mut request = DefineRequest {
        lang: Lang::Rust,
        name: "smoke".into(),
        source: std::fs::read_to_string(source_path)?,
        deps: BTreeMap::new(),
        allowed_effects: None,
    };
    let checker = Checker::new();
    let builder = Builder::new(root, loom_store::Store::memory()?);
    let initial = checker.check(&request).await?;
    if !initial.diagnostics.is_empty() {
        return Err(serde_json::to_string(&initial.diagnostics)?.into());
    }
    request.source = builder
        .prepare_rust_source(&initial, &BTreeMap::new())
        .await?;
    let checked = checker.check(&request).await?;
    let output = builder.build(&checked).await?;
    if !output.diagnostics.is_empty() {
        return Err(serde_json::to_string(&output.diagnostics)?.into());
    }
    std::fs::write(output_path, &output.component)?;
    println!(
        "{}",
        serde_json::json!({"hash":checked.hash,"ms":output.ms,"size":output.component.len()})
    );
    Ok(())
}
