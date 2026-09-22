use super::*;
use serde_json::json;

fn candidate(source: &str) -> Def {
    Def {
        hash: blake3::hash(source.as_bytes()).to_hex().to_string(),
        lang: loom_proto::Lang::Rust,
        component_hash: None,
        sig: Default::default(),
        allowed_effects: None,
        observed_effects: Vec::new(),
    }
}

#[test]
fn session_survives_reopen_and_rejects_stale_agent() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("sessions.db");
    let session = {
        let store = Store::open(&path)?;
        store.create_update_session(&json!({"status":"repairing"}))?
    };
    let first = Store::open(&path)?;
    let second = Store::open(&path)?;
    assert_eq!(first.update_session(&session.id)?, Some(session.clone()));
    let saved = first.save_update_session(&session.id, 0, &json!({"status":"ready"}))?;
    assert_eq!(saved.revision, 1);
    assert!(
        second
            .save_update_session(&session.id, 0, &json!("stale"))
            .is_err()
    );
    assert_eq!(second.update_session(&session.id)?, Some(saved));
    Ok(())
}

#[test]
fn update_rolls_back_earlier_publication_objects_and_session_on_late_failure() -> Result<()> {
    let store = Store::memory()?;
    let session = store.create_update_session(&json!("ready"))?;
    let staged = store.stage_intake()?;
    let object = staged.put("test", b"private")?;
    let first = candidate("first");
    let second = candidate("second");
    let deps = BTreeMap::new();
    let valid = json!({"type":"component_built"});
    let invalid = json!({"type":"invalid"});
    let before = store.latest_seq()?;
    let publications = [
        IntakePublication {
            def: &first,
            name: Some("first"),
            source: "first",
            deps: &deps,
            identity: None,
            build_event: &valid,
        },
        IntakePublication {
            def: &second,
            name: Some("second"),
            source: "second",
            deps: &deps,
            identity: None,
            build_event: &invalid,
        },
    ];
    assert!(
        store
            .commit_update(
                &staged,
                &publications,
                &BTreeMap::new(),
                &session.id,
                0,
                &json!("committed")
            )
            .is_err()
    );
    assert_eq!(store.latest_seq()?, before);
    assert!(store.definition(&first.hash)?.is_none());
    assert!(store.definition(&second.hash)?.is_none());
    assert!(store.get(&object)?.is_none());
    assert!(store.current_names()?.is_empty());
    assert_eq!(store.update_session(&session.id)?, Some(session));
    Ok(())
}

#[test]
fn changed_or_new_names_and_stale_sessions_prevent_publication() -> Result<()> {
    for conflict in ["changed_name", "new_dependent", "stale_session"] {
        let store = Store::memory()?;
        let original = candidate("original");
        let deps = BTreeMap::new();
        store.define(&original, Some("original"), "original", &deps)?;
        let expected_names = store.current_names()?;
        let session = store.create_update_session(&json!("ready"))?;
        let staged = store.stage_intake()?;
        let concurrent = candidate("concurrent");
        match conflict {
            "changed_name" => {
                store.define(&concurrent, Some("original"), "concurrent", &deps)?;
            }
            "new_dependent" => {
                store.define(
                    &concurrent,
                    Some("dependent"),
                    "concurrent",
                    &BTreeMap::from([("original".to_owned(), original.hash.clone())]),
                )?;
            }
            _ => {
                store.save_update_session(&session.id, 0, &json!("changed"))?;
            }
        }
        let before_names = store.current_names()?;
        let before_session = store.update_session(&session.id)?;
        let updated = candidate("updated");
        let event = json!({"type":"component_built"});
        let publication = IntakePublication {
            def: &updated,
            name: Some("original"),
            source: "updated",
            deps: &deps,
            identity: None,
            build_event: &event,
        };
        assert!(
            store
                .commit_update(
                    &staged,
                    &[publication],
                    &expected_names,
                    &session.id,
                    0,
                    &json!("committed")
                )
                .is_err(),
            "{conflict}"
        );
        assert_eq!(store.current_names()?, before_names);
        assert_eq!(store.update_session(&session.id)?, before_session);
        assert!(store.definition(&updated.hash)?.is_none());
    }
    Ok(())
}

#[test]
fn successful_update_retains_old_graph_and_live_effects() -> Result<()> {
    let store = Store::memory()?;
    let old = candidate("old");
    let old_caller = candidate("old caller");
    let deps = BTreeMap::new();
    let old_deps = BTreeMap::from([("function".to_owned(), old.hash.clone())]);
    store.define(&old, Some("function"), "old", &deps)?;
    store.define(&old_caller, Some("caller"), "old caller", &old_deps)?;
    let names = store.current_names()?;
    let session = store.create_update_session(&json!("ready"))?;
    let staged = store.stage_intake()?;
    store.effect_put("effect", "scope", 0, &json!(42))?;
    let new = candidate("new");
    let new_caller = candidate("new caller");
    let new_deps = BTreeMap::from([("function".to_owned(), new.hash.clone())]);
    let event = json!({"type":"component_built"});
    let publications = [
        IntakePublication {
            def: &new,
            name: Some("function"),
            source: "new",
            deps: &deps,
            identity: None,
            build_event: &event,
        },
        IntakePublication {
            def: &new_caller,
            name: Some("caller"),
            source: "new caller",
            deps: &new_deps,
            identity: None,
            build_event: &event,
        },
    ];
    let committed = store.commit_update(
        &staged,
        &publications,
        &names,
        &session.id,
        0,
        &json!("committed"),
    )?;
    assert_eq!(committed.revision, 1);
    assert_eq!(store.update_session(&session.id)?, Some(committed));
    assert_eq!(store.resolve("function")?.unwrap().hash, new.hash);
    assert_eq!(store.resolve("caller")?.unwrap().hash, new_caller.hash);
    assert!(store.definition(&old.hash)?.is_some());
    assert!(store.definition(&old_caller.hash)?.is_some());
    assert_eq!(store.definition_deps(&old_caller.hash)?, old_deps);
    assert_eq!(store.definition_deps(&new_caller.hash)?, new_deps);
    assert_eq!(store.effect_get("effect", "scope", 0)?, Some(json!(42)));
    Ok(())
}

#[test]
fn caller_request_id_is_claimed_once_and_recoverable() -> Result<()> {
    let store = Store::memory()?;
    let session =
        store.create_update_session_with_id("agent-request-1", &json!({"source":"first"}))?;
    assert!(
        store
            .create_update_session_with_id("agent-request-1", &json!({"source":"second"}))
            .is_err()
    );
    assert_eq!(store.update_session("agent-request-1")?, Some(session));
    assert!(store.create_update_session_with_id("", &json!({})).is_err());
    Ok(())
}

#[test]
fn import_commit_is_atomic_and_refuses_moved_names() -> Result<()> {
    let store = Store::memory()?;
    let staged = store.stage_intake()?;
    let object = staged.put("source_bundle", b"imported source")?;
    let first = candidate("import first");
    let second = candidate("import second");
    let deps = BTreeMap::new();
    let valid = json!({"type":"component_built"});
    let names = store.current_names()?;
    let before = store.latest_seq()?;
    let failing = [
        IntakePublication {
            def: &first,
            name: Some("friend/first"),
            source: "first",
            deps: &deps,
            identity: None,
            build_event: &valid,
        },
        IntakePublication {
            def: &second,
            name: Some("friend/second"),
            source: "second",
            deps: &deps,
            identity: None,
            build_event: &json!({"type":"invalid"}),
        },
    ];
    assert!(store.commit_import(&staged, &failing, &names).is_err());
    assert_eq!(store.latest_seq()?, before);
    assert!(store.resolve("friend/first")?.is_none());
    assert!(
        store.get(&object)?.is_none(),
        "rolled back objects stay out"
    );
    let publications = [
        IntakePublication {
            def: &first,
            name: Some("friend/first"),
            source: "first",
            deps: &deps,
            identity: None,
            build_event: &valid,
        },
        IntakePublication {
            def: &second,
            name: Some("friend/second"),
            source: "second",
            deps: &deps,
            identity: None,
            build_event: &valid,
        },
    ];
    // A name bound after planning invalidates the import.
    let moved = candidate("moved");
    store.define(&moved, Some("elsewhere"), "moved", &deps)?;
    let error = store
        .commit_import(&staged, &publications, &names)
        .unwrap_err()
        .to_string();
    assert!(error.contains("import conflict"), "{error}");
    assert!(store.resolve("friend/first")?.is_none());
    let seq = store.commit_import(&staged, &publications, &store.current_names()?)?;
    assert_eq!(seq, store.latest_seq()?);
    assert_eq!(store.resolve("friend/first")?.unwrap().hash, first.hash);
    assert_eq!(store.resolve("friend/second")?.unwrap().hash, second.hash);
    assert_eq!(store.get(&object)?, Some(b"imported source".to_vec()));
    assert!(
        store
            .commit_import(&store, &publications, &store.current_names()?)
            .is_err(),
        "the staged store must be a separate handle"
    );
    Ok(())
}
