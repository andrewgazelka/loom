use super::*;
use loom_proto::Lang;
use serde_json::json;
fn identity(lang: Lang, source: &str, deps: &BTreeMap<String, String>) -> String {
    blake3::hash(&loom_proto::definition_identity(lang, source, deps, None).unwrap())
        .to_hex()
        .to_string()
}
#[test]
fn restart_preserves_definitions_names_and_effects() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("loom.sqlite");
    {
        let store = Store::open(&path)?;
        let hash = store.put("blob", b"same")?;
        assert_eq!(hash, store.put("blob", b"same")?);
        let def = Def {
            hash: identity(Lang::Rust, "source", &BTreeMap::new()),
            lang: Lang::Rust,
            component_hash: None,
            sig: Default::default(),
            allowed_effects: None,
            observed_effects: Vec::new(),
        };
        store.define(&def, Some("counter"), "source", &BTreeMap::new())?;
        assert_eq!(
            store.get(&def.hash)?,
            Some(loom_proto::definition_identity(
                def.lang,
                "source",
                &BTreeMap::new(),
                None
            )?)
        );
        let mut invalid = def.clone();
        invalid.hash = "incorrect".into();
        assert!(
            store
                .define(&invalid, None, "source", &BTreeMap::new())
                .is_err()
        );
        store.effect_put("effect", "global", 0, &json!(42))?;
    }
    let store = Store::open(path)?;
    assert_eq!(
        store.resolve("counter")?.unwrap().hash,
        identity(Lang::Rust, "source", &BTreeMap::new())
    );
    assert_eq!(
        store
            .source(&identity(Lang::Rust, "source", &BTreeMap::new()))?
            .as_deref(),
        Some("source")
    );
    assert_eq!(store.effect_get("effect", "global", 0)?, Some(json!(42)));
    assert!(store.effect_get("effect", "global", 1)?.is_none());
    let seq = store.latest_seq()?;
    store.rebuild_views()?;
    assert_eq!(store.latest_seq()?, seq);
    assert_eq!(
        store.resolve("counter")?.unwrap().hash,
        identity(Lang::Rust, "source", &BTreeMap::new())
    );
    assert_eq!(store.effect_get("effect", "global", 0)?, Some(json!(42)));
    Ok(())
}
#[test]
fn failed_transactions_leave_no_events_or_cas_results() -> Result<()> {
    let store = Store::memory()?;
    store.effect_put("e", "s", 0, &json!(1))?;
    let seq = store.latest_seq()?;
    assert!(store.effect_put("e", "s", 0, &json!(2)).is_err());
    assert_eq!(store.latest_seq()?, seq);
    assert_eq!(store.effect_get("e", "s", 0)?, Some(json!(1)));
    Ok(())
}
#[test]
fn legacy_signatures_migrate_without_changing_historical_events() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("legacy.sqlite");
    let store = Store::open(&path)?;
    let legacy = serde_json::json!({"exports":[{"name":"main","params":[{"name":"left","type":"number"},{"name":"items","type":"unknown[]"}],"returns":"number"}]});
    let source = store.put("source_bundle", b"source")?;
    let hash = identity(Lang::Rust, "source", &BTreeMap::new());
    let event = json!({"type":"defined","def":{"hash":hash,"lang":"rust","component_hash":null,"sig":legacy},"name":"legacy","source_hash":source,"deps":{}});
    let seq = store.record_definition_event(&event)?;
    store.with_connection(|c| {
        c.execute(
            "INSERT INTO defs(hash,lang,name_hint,type_sig,component_hash,source_hash) VALUES (?,'rust','legacy',?,NULL,?)",
            params![hash, serde_json::to_string(&legacy)?, source],
        )?;
        c.execute(
            "INSERT INTO names VALUES ('legacy',?,?)",
            params![hash, seq],
        )?;
        Ok(())
    })?;
    let old_event_hash = blake3::hash(&encode(&event)?).to_hex().to_string();
    drop(store);
    let store = Store::open(&path)?;
    assert_eq!(
        store.definition(&hash)?.unwrap().sig.exports[0].returns,
        loom_proto::ValueShape::Number
    );
    assert_eq!(store.get_value::<Value>(&old_event_hash)?, Some(event));
    assert_eq!(
        store.get(&hash)?,
        Some(loom_proto::definition_identity(
            Lang::Rust,
            "source",
            &BTreeMap::new(),
            None
        )?)
    );
    let seq = store.latest_seq()?;
    store.rebuild_views()?;
    assert_eq!(
        store.resolve("legacy")?.unwrap().sig.exports[0].params[0].shape,
        loom_proto::ValueShape::Number
    );
    drop(store);
    let store = Store::open(path)?;
    assert_eq!(store.latest_seq()?, seq);
    Ok(())
}
#[test]
fn name_history_keeps_old_definition_and_dependencies() -> Result<()> {
    let store = Store::memory()?;
    let deps = BTreeMap::from_iter([("dep".into(), "target".into())]);
    let old = identity(Lang::Rust, "old", &deps);
    let new = identity(Lang::Rust, "new", &deps);
    for source in ["old", "new"] {
        let hash = identity(Lang::Rust, source, &deps);
        store.define(
            &Def {
                hash,
                lang: Lang::Rust,
                component_hash: None,
                sig: Default::default(),
                allowed_effects: None,
                observed_effects: Vec::new(),
            },
            Some("name"),
            source,
            &deps,
        )?;
    }
    assert_eq!(store.resolve("name")?.unwrap().hash, new);
    assert!(store.definition(&old)?.is_some());
    assert_eq!(store.dependencies(&new)?, vec!["target"]);
    let mut expected = vec![old, new.clone()];
    expected.sort();
    assert_eq!(store.dependents("target")?, expected);
    assert_eq!(store.definition_deps(&new)?["dep"], "target");
    assert_eq!(store.name_history("name")?.len(), 2);
    Ok(())
}

#[test]
fn definition_recording_preserves_replay_and_rebuilds_effect_cache() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("records.sqlite");
    let store = Store::open(&path)?;
    let hash = identity(Lang::Rust, "source", &BTreeMap::new());
    store.define(
        &Def {
            hash: hash.clone(),
            lang: Lang::Rust,
            component_hash: None,
            sig: Default::default(),
            allowed_effects: None,
            observed_effects: Vec::new(),
        },
        Some("d"),
        "source",
        &BTreeMap::new(),
    )?;
    store.effect_put("desc", "global", 0, &json!(123))?;
    let before = serde_json::to_value(store.definition_events(0, 1000)?)?;
    drop(store);
    let store = Store::open(&path)?;
    assert_eq!(
        serde_json::to_value(store.definition_events(0, 1000)?)?,
        before
    );
    store.rebuild_views()?;
    assert_eq!(store.resolve("d")?.unwrap().hash, hash);
    store.with_connection(|connection| {
        connection.execute("DELETE FROM effect_results", [])?;
        Ok(())
    })?;
    drop(store);
    let store = Store::open(path)?;
    store.rebuild_views()?;
    assert_eq!(store.effect_get("desc", "global", 0)?, Some(json!(123)));
    assert!(store.effect_put("desc", "global", 0, &json!(124)).is_err());
    Ok(())
}
