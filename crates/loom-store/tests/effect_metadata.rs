use anyhow::Result;
use loom_proto::{Def, Lang, definition_identity};
use loom_store::Store;
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn explicit_policy_changes_identity_but_legacy_identity_is_unchanged() -> Result<()> {
    let deps = BTreeMap::new();
    let none = definition_identity(Lang::Ts, "source", &deps, None)?;
    assert_eq!(
        none,
        serde_json::to_vec(&json!({"version":1,"lang":"ts","source":"source","deps":{}}))?
    );
    assert_ne!(
        none,
        definition_identity(Lang::Ts, "source", &deps, Some(&[]))?
    );
    let unsorted = vec!["fs.read".into(), "exec".into(), "fs.read".into()];
    let sorted = vec!["exec".into(), "fs.read".into()];
    assert_eq!(
        definition_identity(Lang::Ts, "source", &deps, Some(&unsorted))?,
        definition_identity(Lang::Ts, "source", &deps, Some(&sorted))?
    );
    Ok(())
}

#[test]
fn policy_and_observations_survive_restart_archive_and_projection_rebuild() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("effects.sqlite");
    let deps = BTreeMap::new();
    let policy = vec!["fs.read".into(), "exec".into(), "fs.read".into()];
    let hash = blake3::hash(&definition_identity(
        Lang::Rust,
        "source",
        &deps,
        Some(&policy),
    )?)
    .to_hex()
    .to_string();
    {
        let store = Store::open(&path)?;
        store.define(
            &Def {
                hash: hash.clone(),
                lang: Lang::Rust,
                component_hash: None,
                sig: Default::default(),
                allowed_effects: Some(policy),
                observed_effects: vec!["forged".into()],
            },
            Some("worker"),
            "source",
            &deps,
        )?;
        assert!(
            store
                .definition(&hash)?
                .unwrap()
                .observed_effects
                .is_empty()
        );
        for op in ["fs.read", "fs.read", "exec"] {
            store.append(
                "system",
                &json!({"type":"effect_invoked","def_hash":hash,"op":op}),
                0,
            )?;
        }
        store.append(
            "system",
            &json!({"type":"effect_invoked","def_hash":"other","op":"random"}),
            0,
        )?;
        store.compact_log(store.latest_seq()?, 1000)?;
    }
    let store = Store::open(path)?;
    for rebuild in [false, true] {
        if rebuild {
            store.rebuild_views()?;
        }
        let def = store.definition(&hash)?.unwrap();
        assert_eq!(
            def.allowed_effects,
            Some(vec!["exec".into(), "fs.read".into()])
        );
        assert_eq!(def.observed_effects, vec!["exec", "fs.read"]);
    }
    Ok(())
}

#[test]
fn trusted_host_effects_remain_logged_without_a_definition_projection() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("host.sqlite");
    let event = json!({"type":"effect_invoked","def_hash":null,"op":"exec"});
    {
        let store = Store::open(&path)?;
        store.append("system", &event, 0)?;
        store.rebuild_views()?;
    }
    let store = Store::open(path)?;
    assert_eq!(store.events(Some("system"), 0, 10)?[0].event, event);
    let count: i64 = store.with_connection(|c| {
        Ok(c.query_row("SELECT count(*) FROM def_effects", [], |r| r.get(0))?)
    })?;
    assert_eq!(count, 0);
    Ok(())
}

#[test]
fn executable_metadata_excludes_recorded_observations() -> Result<()> {
    let store = Store::memory()?;
    let deps = BTreeMap::new();
    let policy = vec!["fs.read".to_owned()];
    let hash = blake3::hash(&definition_identity(
        Lang::Rust,
        "source",
        &deps,
        Some(&policy),
    )?)
    .to_hex()
    .to_string();
    store.define(
        &Def {
            hash: hash.clone(),
            lang: Lang::Rust,
            component_hash: Some("artifact".into()),
            sig: Default::default(),
            allowed_effects: Some(policy.clone()),
            observed_effects: Vec::new(),
        },
        None,
        "source",
        &deps,
    )?;
    store.enqueue_recording(&json!({"type":"effect_invoked","def_hash":hash,"op":"fs.read"}))?;
    let executable = store.executable_definition(&hash)?.unwrap();
    assert_eq!(executable.hash, hash);
    assert_eq!(executable.component_hash.as_deref(), Some("artifact"));
    assert_eq!(executable.allowed_effects, Some(policy));
    assert!(executable.observed_effects.is_empty());
    assert_eq!(
        store.definition(&hash)?.unwrap().observed_effects,
        vec!["fs.read"]
    );
    assert!(store.executable_definition("missing")?.is_none());
    Ok(())
}
