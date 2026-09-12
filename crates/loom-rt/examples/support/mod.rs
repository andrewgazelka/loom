use anyhow::Result;
use loom_proto::{Def, Lang, definition_identity};
use loom_store::Store;
use std::{collections::BTreeMap, path::Path};

/// Register a prebuilt core wasm module using the same identity contract as checked
/// source. Include its content hash so distinct compiled fixtures stay distinct.
pub fn register(store: &Store, lang: Lang, path: impl AsRef<Path>, label: &str) -> Result<String> {
    let component = std::fs::read(path)?;
    anyhow::ensure!(
        loom_proto::core_protocol::is_current(&component),
        "fixture {label} requires an admitted core wasm artifact produced by loom-build"
    );
    let component_hash = store.put("component", &component)?;
    let source = format!("{label}: {component_hash}");
    let deps = BTreeMap::new();
    let hash = blake3::hash(&definition_identity(lang, &source, &deps, None)?)
        .to_hex()
        .to_string();
    store.define(
        &Def {
            hash: hash.clone(),
            lang,
            component_hash: Some(component_hash),
            sig: Default::default(),
            allowed_effects: None,
            observed_effects: Vec::new(),
        },
        None,
        &source,
        &deps,
    )?;
    Ok(hash)
}
