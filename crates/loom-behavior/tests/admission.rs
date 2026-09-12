use std::{collections::BTreeMap, sync::Arc};

use loom_actor::{Config, DefaultEffects, Node};
use loom_proto::{Def, ExportSig, Lang, ParamSig, TypeSig, ValueShape};
use loom_store::Store;

#[tokio::test]
async fn spawn_rejects_multiple_entries_before_creating_actor() -> anyhow::Result<()> {
    let store = Store::memory()?;
    let definition = Def {
        hash: blake3::hash(b"two actor entries").to_hex().to_string(),
        lang: Lang::Rust,
        // Admission must reject the signature before loading the executable.
        component_hash: None,
        sig: TypeSig {
            exports: ["alpha", "beta"]
                .into_iter()
                .map(|name| ExportSig {
                    name: name.to_owned(),
                    params: vec![ParamSig {
                        name: "message".to_owned(),
                        shape: ValueShape::Array {
                            items: Box::new(ValueShape::Number),
                        },
                    }],
                    returns: ValueShape::Null,
                    effects: Default::default(),
                })
                .collect(),
            effects: Default::default(),
        },
        allowed_effects: None,
        observed_effects: Vec::new(),
    };
    store.define(&definition, Some("two_entries"), "source", &BTreeMap::new())?;
    let directory = tempfile::tempdir()?;
    let node = Node::new(
        directory.path(),
        Arc::new(loom_behavior::StoreRegistry::new(store)),
        Arc::new(DefaultEffects),
        Config::default(),
    )
    .await?;
    let before = node.actor_ids()?;
    let error = match node.spawn_root("two_entries", b"{}").await {
        Ok(_) => anyhow::bail!("multiple-entry actor unexpectedly admitted"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("exactly one entry"), "{error}");
    assert!(error.contains("alpha"), "{error}");
    assert!(error.contains("beta"), "{error}");
    assert_eq!(node.actor_ids()?, before);
    Ok(())
}
