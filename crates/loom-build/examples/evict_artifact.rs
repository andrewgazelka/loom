//! Destructive cache control for an isolated benchmark database, never a daemon RPC.
use loom_store::Store;
use serde::Deserialize;
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Deserialize)]
struct BuildMetadata {
    dependency_graph: String,
}
#[derive(Deserialize)]
struct Graph {
    units: Vec<Unit>,
}
#[derive(Deserialize)]
struct Unit {
    name: String,
    key: String,
    outputs: Vec<Output>,
    recipe: Recipe,
}
#[derive(Deserialize)]
struct Recipe {
    arguments: Vec<String>,
}
#[derive(Deserialize)]
struct Output {
    path: PathBuf,
    hash: String,
}

fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let database = arguments.next().ok_or_else(|| {
        anyhow::anyhow!("usage: evict_artifact ISOLATED_DB CRATE_NAME TARGET BUILD_LOG_HASH")
    })?;
    let name = arguments
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing crate name"))?;
    let target = arguments
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing compiler target"))?;
    let log_hash = arguments
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing build log hash"))?;
    anyhow::ensure!(arguments.next().is_none(), "unexpected argument");
    let store = Store::open(database)?;
    let logs = store
        .get(&log_hash)?
        .ok_or_else(|| anyhow::anyhow!("build log missing"))?;
    let keys: BTreeSet<String> = std::str::from_utf8(&logs)?
        .lines()
        .filter_map(|line| serde_json::from_str::<BuildMetadata>(line).ok())
        .map(|metadata| metadata.dependency_graph)
        .collect();
    anyhow::ensure!(
        keys.len() == 1,
        "build log must identify exactly one dependency graph"
    );
    let key = keys.into_iter().next().expect("one graph checked");
    anyhow::ensure!(
        key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid graph key"
    );
    let manifest: String = store.with_connection(|connection| {
        Ok(connection.query_row(
            "SELECT recipe_hash FROM rust_build_graphs WHERE key=?",
            [&key],
            |row| row.get(0),
        )?)
    })?;
    let graph: Graph = store
        .get_value(&manifest)?
        .ok_or_else(|| anyhow::anyhow!("build graph missing"))?;
    let mut matching = graph.units.into_iter().filter(|unit| {
        unit.name == name
            && unit
                .recipe
                .arguments
                .windows(2)
                .any(|arguments| arguments[0] == "--target" && arguments[1] == target)
    });
    if let Some(artifact) = matching.next() {
        anyhow::ensure!(
            matching.next().is_none(),
            "crate name and compiler target are ambiguous within dependency graph"
        );
        anyhow::ensure!(!artifact.outputs.is_empty(), "crate has no artifacts");
        for output in &artifact.outputs {
            let hash = &output.hash;
            anyhow::ensure!(
                store.get(hash)?.is_some(),
                "target artifact is already absent from CAS"
            );
            if output.path.exists() {
                let bytes = std::fs::read(&output.path)?;
                anyhow::ensure!(
                    blake3::hash(&bytes).to_hex().as_str() == hash,
                    "materialized artifact differs from target graph"
                );
            }
        }
        let mut removed = 0;
        for output in artifact.outputs {
            let hash = &output.hash;
            if output.path.exists() {
                std::fs::remove_file(&output.path)?;
            }
            store.with_connection(|connection| {
                connection.execute("DELETE FROM cas WHERE hash=?", [hash])?;
                Ok(())
            })?;
            anyhow::ensure!(
                store.get(hash)?.is_none(),
                "CAS eviction did not take effect"
            );
            removed += 1;
        }
        anyhow::ensure!(removed > 0, "crate has no artifacts");
        println!(
            "{}",
            serde_json::json!({"name":name,"target":target,"key":artifact.key,"dependency_graph":key,"build_log_hash":log_hash,"removed_outputs":removed})
        );
        return Ok(());
    }
    anyhow::bail!("compiled crate {name} for {target} not found in graph {key}")
}
