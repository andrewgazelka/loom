use super::*;
use loom_proto::Lang;
use serde_json::json;
fn identity(lang: Lang, source: &str, deps: &BTreeMap<String, String>) -> String {
    blake3::hash(&loom_proto::definition_identity(lang, source, deps, None).unwrap())
        .to_hex()
        .to_string()
}
fn actor() -> Actor {
    Actor {
        id: "a".into(),
        behavior_hash: "b".into(),
        lang: Lang::Rust,
        component_hash: None,
        last_seq: 0,
        created_seq: 0,
        parent: None,
    }
}
#[test]
fn restart_preserves_events_names_snapshots_and_effects() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("loom.sqlite");
    let final_seq;
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
        store.create_actor(&actor())?;
        final_seq = store.append_batch("a", &[json!(1), json!(2)], 7)?;
        store.snapshot("a", "fold1", final_seq, &json!(3))?;
        store.effect_put("effect", "global", 0, &json!(42))?;
        store.create_session("s", "a", "owner")?;
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
    assert_eq!(store.events(Some("a"), 0, 100)?.len(), 2);
    assert_eq!(store.actor("a")?.unwrap().last_seq, final_seq);
    assert_eq!(
        store.latest_snapshot("a", "fold1")?.unwrap().state,
        json!(3)
    );
    assert!(store.latest_snapshot("a", "fold2")?.is_none());
    assert_eq!(store.effect_get("effect", "global", 0)?, Some(json!(42)));
    assert!(store.effect_get("effect", "global", 1)?.is_none());
    assert_eq!(store.session("s")?.as_deref(), Some("a"));
    let seq = store.latest_seq()?;
    store.rebuild_views()?;
    assert_eq!(store.latest_seq()?, seq);
    assert_eq!(
        store.resolve("counter")?.unwrap().hash,
        identity(Lang::Rust, "source", &BTreeMap::new())
    );
    assert_eq!(store.actor("a")?.unwrap().last_seq, final_seq);
    assert_eq!(store.effect_get("effect", "global", 0)?, Some(json!(42)));
    assert_eq!(store.session("s")?.as_deref(), Some("a"));
    Ok(())
}
#[test]
fn failed_transactions_leave_no_events_or_cas_results() -> Result<()> {
    let store = Store::memory()?;
    store.create_actor(&actor())?;
    let seq = store.latest_seq()?;
    assert!(store.create_actor(&actor()).is_err());
    assert_eq!(store.latest_seq()?, seq);
    assert!(store.append_batch("missing", &[json!(1)], 0).is_err());
    assert_eq!(store.latest_seq()?, seq);
    store.effect_put("e", "s", 0, &json!(1))?;
    let seq = store.latest_seq()?;
    assert!(store.effect_put("e", "s", 0, &json!(2)).is_err());
    assert_eq!(store.latest_seq()?, seq);
    assert_eq!(store.effect_get("e", "s", 0)?, Some(json!(1)));
    Ok(())
}
#[test]
fn initialized_actor_is_atomic_and_recoverable() -> Result<()> {
    let store = Store::memory()?;
    let created = store.create_initialized_actor(&actor(), &json!({"count":3}))?;
    assert_eq!(store.events(Some("a"), 0, 100)?.len(), 1);
    assert_eq!(
        store.latest_snapshot("a", "b")?.unwrap().state,
        json!({"count":3})
    );
    let seq = store.latest_seq()?;
    assert!(
        store
            .create_initialized_actor(&actor(), &json!("wrong"))
            .is_err()
    );
    assert_eq!(store.latest_seq()?, seq);
    store.rebuild_views()?;
    assert_eq!(store.actor("a")?.unwrap().created_seq, created.created_seq);
    assert_eq!(store.actor("a")?.unwrap().last_seq, created.last_seq);
    assert_eq!(
        store.events(Some("a"), 0, 100)?[0].event,
        json!({"__loom_init":{"count":3}})
    );
    Ok(())
}
#[test]
fn mailbox_migrates_and_delivers_duplicate_messages_in_order() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("mailbox.sqlite");
    let store = Store::open(&path)?;
    store.create_actor(&actor())?;
    let first = store.enqueue("a", &json!(1))?;
    store.with_connection(|c| {c.execute_batch("CREATE TABLE inbox_old(actor TEXT PRIMARY KEY REFERENCES actors(id),handler_seq INTEGER NOT NULL REFERENCES log(seq),msg TEXT NOT NULL); INSERT INTO inbox_old SELECT * FROM inbox; DROP TABLE inbox; ALTER TABLE inbox_old RENAME TO inbox;")?;Ok(())})?;
    drop(store);
    let store = Store::open(path)?;
    let second = store.enqueue("a", &json!(1))?;
    assert!(second.handler_seq > first.handler_seq);
    assert_eq!(store.pending_messages()?.len(), 2);
    assert!(
        store
            .complete_message("a", second.handler_seq, &[json!("bad")])
            .is_err()
    );
    store.rebuild_views()?;
    assert_eq!(store.pending("a")?.unwrap().handler_seq, first.handler_seq);
    store.complete_message("a", first.handler_seq, &[json!(1)])?;
    assert!(!store.message_pending("a", first.handler_seq)?);
    assert!(store.message_pending("a", second.handler_seq)?);
    store.rebuild_views()?;
    assert_eq!(store.pending("a")?.unwrap().handler_seq, second.handler_seq);
    store.complete_message("a", second.handler_seq, &[json!(2)])?;
    assert!(store.pending_messages()?.is_empty());
    let receipt = store.enqueue_once("a", &json!(9), "sender:effect:0")?;
    assert_eq!(
        store
            .enqueue_once("a", &json!(9), "sender:effect:0")?
            .handler_seq,
        receipt.handler_seq
    );
    store.complete_message("a", receipt.handler_seq, &[])?;
    store.compact_log(store.latest_seq()?, 1000)?;
    store.rebuild_views()?;
    assert_eq!(
        store
            .enqueue_once("a", &json!(9), "sender:effect:0")?
            .handler_seq,
        receipt.handler_seq
    );
    assert!(!store.message_pending("a", receipt.handler_seq)?);
    assert!(
        store
            .enqueue_once("a", &json!(10), "sender:effect:0")
            .is_err()
    );
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
    let seq = store.append("system", &event, 0)?;
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
    store.create_actor(&actor())?;
    store.append("a", &json!(3), 0)?;
    store.create_session("session", "a", "owner")?;
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
    assert_eq!(store.session("session")?.as_deref(), Some("a"));
    assert_eq!(
        store.events(Some("a"), 0, 100)?.last().unwrap().event,
        json!(3)
    );
    drop(store);
    let store = Store::open(path)?;
    assert_eq!(store.latest_seq()?, seq);
    Ok(())
}
#[test]
fn archive_compaction_preserves_replay_and_pending_delivery() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("compact.sqlite");
    let store = Store::open(&path)?;
    store.define(
        &Def {
            hash: identity(Lang::Rust, "source", &BTreeMap::new()),
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
    store.create_actor(&actor())?;
    for n in 0..100 {
        store.append(
            "a",
            &json!({"n":n,"data":"repeat this text to compress"}),
            0,
        )?;
    }
    store.effect_put("desc", "global", 0, &json!(123))?;
    let message = store.enqueue("a", &json!({"msg":1}))?;
    let before = serde_json::to_value(store.events(None, 0, 1000)?)?;
    let event_bytes = encode(&json!({"n":0,"data":"repeat this text to compress"}))?;
    let event_hash = blake3::hash(&event_bytes).to_hex().to_string();
    let first = store.compact_log(50, 1000)?;
    assert!(first.events > 0);
    let second = store.compact_log(store.latest_seq()?, 1000)?;
    assert!(second.events > 0);
    assert!(second.after_bytes < second.before_bytes);
    assert_eq!(serde_json::to_value(store.events(None, 0, 1000)?)?, before);
    assert_eq!(store.compact_log(store.latest_seq()?, 1000)?.events, 0);
    assert_eq!(store.get(&event_hash)?, Some(event_bytes));
    drop(store);
    let store = Store::open(&path)?;
    assert_eq!(serde_json::to_value(store.events(None, 0, 1000)?)?, before);
    store.rebuild_views()?;
    assert_eq!(
        store.resolve("d")?.unwrap().hash,
        identity(Lang::Rust, "source", &BTreeMap::new())
    );
    assert_eq!(
        store.pending("a")?.unwrap().handler_seq,
        message.handler_seq
    );
    store.with_connection(|c| {
        c.execute("DELETE FROM effect_results", [])?;
        Ok(())
    })?;
    drop(store);
    let store = Store::open(path)?;
    store.rebuild_views()?;
    assert_eq!(store.effect_get("desc", "global", 0)?, Some(json!(123)));
    assert!(store.effect_put("desc", "global", 0, &json!(124)).is_err());
    store.complete_message("a", message.handler_seq, &[json!("done")])?;
    assert!(store.pending("a")?.is_none());
    store.rebuild_views()?;
    assert!(store.pending("a")?.is_none());
    assert_eq!(store.events(Some("a"), 0, 1000)?.len(), 101);
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
