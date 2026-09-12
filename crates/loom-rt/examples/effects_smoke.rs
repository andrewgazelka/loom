use anyhow::{Result, ensure};
use loom_proto::{Def, Lang, definition_identity};
use loom_rt::Runtime;
use loom_store::Store;
use serde_json::json;
use std::collections::BTreeMap;

fn register(store: &Store, component: &str, allowed: Option<Vec<String>>) -> Result<String> {
    let source = format!("effects fixture {component}");
    let deps = BTreeMap::new();
    let hash = blake3::hash(&definition_identity(
        Lang::Rust,
        &source,
        &deps,
        allowed.as_deref(),
    )?)
    .to_hex()
    .to_string();
    store.define(
        &Def {
            hash: hash.clone(),
            lang: Lang::Rust,
            component_hash: Some(component.into()),
            sig: Default::default(),
            allowed_effects: allowed,
            observed_effects: Vec::new(),
        },
        None,
        &source,
        &deps,
    )?;
    Ok(hash)
}
#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args().nth(1).expect("effects component path");
    let store = Store::memory()?;
    let component = store.put("component", &std::fs::read(path)?)?;
    let open = register(&store, &component, None)?;
    let denied = register(&store, &component, Some(vec![]))?;
    let caller = register(&store, &component, Some(vec!["call".into()]))?;
    let runtime = Runtime::new(store.clone())?;
    let leaf = json!({"op":"cas.put","args":{"secret":42}});
    let warm = runtime.call_def(&open, json!([leaf])).await?;
    ensure!(warm["ok"] == true, "warm: {warm}");
    let blocked = runtime.call_def(&denied, json!([leaf])).await?;
    ensure!(
        blocked["ok"] == false
            && blocked["error"]
                .as_str()
                .unwrap_or("")
                .contains("not allowed"),
        "cached denial: {blocked}"
    );
    let nested = json!({"op":"call","args":{"def":open,"args":[leaf]}});
    let result = runtime.call_def(&caller, json!([nested])).await?;
    ensure!(
        result["ok"] == true && result["value"]["ok"] == false,
        "nested: {result}"
    );
    ensure!(
        store
            .definition(&open)?
            .unwrap()
            .observed_effects
            .contains(&"cas.put".into())
    );
    ensure!(
        store
            .definition(&denied)?
            .unwrap()
            .observed_effects
            .is_empty()
    );
    println!("effects native 3/3: permitted CAS, cached denial, delegated call");
    Ok(())
}
