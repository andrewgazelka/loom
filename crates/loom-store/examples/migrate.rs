use anyhow::{Context, Result, ensure};
use loom_store::Store;
fn main() -> Result<()> {
    let path = std::env::args().nth(1).context("usage: migrate DATABASE")?;
    let store = Store::open(path)?;
    let defs = store.definitions()?;
    for def in &defs {
        let bytes = store
            .get(&def.hash)?
            .context("definition absent from CAS")?;
        ensure!(
            blake3::hash(&bytes).to_hex().as_str() == def.hash,
            "CAS identity mismatch"
        );
    }
    let actors = serde_json::to_value(store.actors()?)?;
    let events = serde_json::to_value(store.events(None, 0, 1000)?)?;
    store.rebuild_views()?;
    ensure!(
        serde_json::to_value(store.actors()?)? == actors,
        "actor projection changed"
    );
    ensure!(
        serde_json::to_value(store.events(None, 0, 1000)?)? == events,
        "historical events changed"
    );
    ensure!(
        store.definitions()?.len() == defs.len(),
        "definition count changed"
    );
    println!(
        "{} definitions migrated; actor projections and immutable events verified",
        defs.len()
    );
    Ok(())
}
