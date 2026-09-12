use anyhow::{Context, Result, ensure};
use loom_build::Builder;
use loom_check::CheckedDef;
use loom_proto::{Def, Lang, definition_identity};
use loom_store::Store;
use serde::Serialize;
use std::{collections::BTreeMap, path::PathBuf, process::ExitCode};

#[derive(Serialize)]
struct FixtureHashes {
    handler: String,
    promoted: String,
    no_schema: String,
}

fn main() -> Result<ExitCode> {
    if let Some(status) = loom_build::compiler_cache_entry()? {
        return Ok(status);
    }
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments
        .next()
        .context("expected definitions database path")?;
    ensure!(
        arguments.next().is_none(),
        "expected exactly one definitions database path"
    );
    let hashes = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(build(PathBuf::from(first)))?;
    println!("{}", serde_json::to_string(&hashes)?);
    Ok(ExitCode::SUCCESS)
}

async fn build(database: PathBuf) -> Result<FixtureHashes> {
    let store = Store::open(database)?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("crate parent")?
        .parent()
        .context("workspace root")?
        .to_owned();
    let builder = Builder::new(root, store.clone());
    let handler = compile(
        &builder,
        &store,
        "handler",
        include_str!("fixtures/handler.rs"),
    )
    .await?;
    let promoted = compile(
        &builder,
        &store,
        "promoted",
        include_str!("fixtures/promoted.rs"),
    )
    .await?;
    let no_schema = compile(
        &builder,
        &store,
        "no_schema",
        include_str!("fixtures/no_schema.rs"),
    )
    .await?;
    Ok(FixtureHashes {
        handler,
        promoted,
        no_schema,
    })
}

async fn compile(builder: &Builder, store: &Store, name: &str, source: &str) -> Result<String> {
    let deps = BTreeMap::new();
    let mut checked = CheckedDef {
        hash: identity(source)?,
        lang: Lang::Rust,
        name: name.into(),
        source: source.into(),
        deps: deps.clone(),
        sig: Default::default(),
        diagnostics: Vec::new(),
    };
    checked.source = builder
        .prepare_rust_source(&checked, &BTreeMap::new())
        .await
        .with_context(|| format!("prepare fixture {name}"))?;
    checked.hash = identity(&checked.source)?;
    let built = builder
        .build(&checked)
        .await
        .with_context(|| format!("build fixture {name}"))?;
    ensure!(
        built.diagnostics.is_empty(),
        "fixture {name}: {:?}\n{}",
        built.diagnostics,
        built.logs
    );
    ensure!(
        !built.component.is_empty(),
        "fixture {name}: empty compiled module"
    );
    let component_hash = store.put("component", &built.component)?;
    store.define(
        &Def {
            hash: checked.hash.clone(),
            lang: Lang::Rust,
            component_hash: Some(component_hash),
            sig: checked.sig,
            allowed_effects: None,
            observed_effects: Vec::new(),
        },
        None,
        &checked.source,
        &deps,
    )?;
    Ok(checked.hash)
}

fn identity(source: &str) -> Result<String> {
    Ok(blake3::hash(&definition_identity(
        Lang::Rust,
        source,
        &BTreeMap::new(),
        None,
    )?)
    .to_hex()
    .to_string())
}
