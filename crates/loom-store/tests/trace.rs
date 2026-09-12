use anyhow::Result;
use loom_proto::{
    CallTrace, TraceBlob, TraceBlobKind, TraceBundle, TraceEntry, TraceKey, TraceOutcome,
};
use loom_store::Store;
use serde_json::json;

fn blob(kind: TraceBlobKind, value: serde_json::Value) -> Result<TraceBlob> {
    let bytes = loom_proto::encode(&value).map_err(anyhow::Error::msg)?;
    Ok(TraceBlob {
        hash: blake3::hash(&bytes).to_hex().to_string(),
        kind,
        bytes,
    })
}
fn bundle(scope: &str) -> Result<TraceBundle> {
    let descriptor = blob(TraceBlobKind::Descriptor, json!({"op":"fs.list","args":{}}))?;
    let result = blob(TraceBlobKind::Result, json!(["one"]))?;
    Ok(TraceBundle {
        trace: CallTrace {
            version: 1,
            definition_hash: None,
            args_hash: None,
            scope: scope.into(),
            entries: vec![TraceEntry {
                key: TraceKey {
                    scope: scope.into(),
                    occurrence: 0,
                },
                descriptor_hash: descriptor.hash.clone(),
                outcome: TraceOutcome::Success {
                    result_hash: result.hash.clone(),
                },
            }],
            outcome: Some(TraceOutcome::Success {
                result_hash: result.hash.clone(),
            }),
        },
        blobs: vec![descriptor, result],
        memos: vec![],
        observations: vec![],
    })
}
#[test]
fn queued_trace_reopens_with_errors_and_pages_immutable_history() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("trace.sqlite");
    let store = Store::open(&path)?;
    let mut first = bundle("call")?;
    first.trace.outcome = None;
    let old_hash = store.persist_call_trace(&first)?;
    store.flush()?;
    let mut final_trace = first.clone();
    final_trace.trace.entries.push(TraceEntry {
        key: TraceKey {
            scope: "call/child".into(),
            occurrence: 0,
        },
        descriptor_hash: first.trace.entries[0].descriptor_hash.clone(),
        outcome: TraceOutcome::Error {
            message: "failure".into(),
        },
    });
    final_trace.trace.outcome = Some(TraceOutcome::Error {
        message: "failure".into(),
    });
    let hash = store.persist_call_trace(&final_trace)?;
    store.flush()?;
    assert_eq!(store.recording_commit_count(), 2);
    assert_eq!(store.recording_timings().traces, 2);
    assert!(store.recording_timings().trace_bytes > 0);
    assert_eq!(store.trace_effects(&old_hash, 0, 256)?.entries.len(), 1);
    let page = store.trace_effects(&hash, 0, 1)?;
    assert_eq!(page.next_offset, Some(1));
    assert_eq!(page.entries[0].op, "fs.list");
    assert!(store.trace_effects(&hash, 0, 257).is_err());
    assert!(store.trace_effects(&hash, 3, 1).is_err());
    drop(store);
    let store = Store::open(&path)?;
    assert_eq!(
        store.load_call_trace("call")?.unwrap().trace,
        final_trace.trace
    );
    store.rebuild_views()?;
    assert_eq!(
        store.load_call_trace("call")?.unwrap().trace,
        final_trace.trace
    );
    Ok(())
}
#[test]
fn legacy_migration_is_verified_once_and_preserves_recovery() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("legacy.sqlite");
    let store = Store::open(&path)?;
    let data = bundle("legacy/child")?;
    let descriptor = store.put_value("desc", &json!({"op":"fs.list","args":{}}))?;
    store.effect_put(&descriptor, "legacy/child", 0, &json!(["one"]))?;
    store.with_connection(|connection| {
        connection.execute(
            "DELETE FROM store_migrations WHERE name='call_trace_v2'",
            [],
        )?;
        Ok(())
    })?;
    let before = store.latest_seq()?;
    drop(store);
    let store = Store::open(&path)?;
    let migrated = store.load_call_trace("legacy")?.unwrap();
    assert_eq!(migrated.trace.entries, data.trace.entries);
    assert!(migrated.trace.outcome.is_none());
    assert!(store.effect_get(&descriptor, "legacy/child", 0)?.is_none());
    assert_eq!(store.latest_seq()?, before + 1);
    let after = store.latest_seq()?;
    drop(store);
    let store = Store::open(&path)?;
    assert_eq!(store.latest_seq()?, after);
    store.rebuild_views()?;
    assert_eq!(
        store.load_call_trace("legacy")?.unwrap().trace,
        migrated.trace
    );
    Ok(())
}
#[test]
fn checkpoint_cannot_remove_a_recorded_occurrence() -> Result<()> {
    let store = Store::memory()?;
    let mut data = bundle("call")?;
    data.trace.outcome = None;
    store.persist_call_trace(&data)?;
    store.flush()?;
    data.trace.entries.clear();
    store.persist_call_trace(&data)?;
    assert!(
        store
            .flush()
            .unwrap_err()
            .to_string()
            .contains("removed an occurrence")
    );
    Ok(())
}

#[test]
fn legacy_failure_without_result_migrates_into_recovery_trace() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("failure.sqlite");
    let store = Store::open(&path)?;
    let descriptor = store.put_value("desc", &json!({"op":"fs.read","args":{}}))?;
    store.record_definition_event( &json!({"type":"effect_completed","scope":"failed/child","occurrence":2,"desc_hash":descriptor,"error":"permission denied"}))?;
    store.with_connection(|connection| {
        connection.execute(
            "DELETE FROM store_migrations WHERE name='call_trace_v2'",
            [],
        )?;
        Ok(())
    })?;
    drop(store);
    let store = Store::open(&path)?;
    let trace = store.load_call_trace("failed")?.unwrap().trace;
    assert_eq!(
        trace.entries[0].outcome,
        TraceOutcome::Error {
            message: "permission denied".into()
        }
    );
    assert!(trace.outcome.is_none());
    Ok(())
}

#[test]
fn ambiguous_legacy_occurrence_rolls_back_migration_and_marker() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("ambiguous.sqlite");
    let store = Store::open(&path)?;
    for op in ["fs.list", "fs.read"] {
        let descriptor = store.put_value("desc", &json!({"op":op,"args":{}}))?;
        store.effect_put(&descriptor, "legacy", 0, &json!(1))?;
    }
    store.with_connection(|connection| {
        connection.execute(
            "DELETE FROM store_migrations WHERE name='call_trace_v2'",
            [],
        )?;
        Ok(())
    })?;
    drop(store);
    assert!(Store::open(&path).is_err());
    let connection = rusqlite::Connection::open(&path)?;
    assert_eq!(
        connection.query_row("SELECT count(*) FROM effect_results", [], |row| row
            .get::<_, i64>(0))?,
        2
    );
    assert_eq!(
        connection.query_row(
            "SELECT count(*) FROM store_migrations WHERE name='call_trace_v2'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

#[test]
fn invalid_trace_rolls_back_all_blobs_and_completion_event() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("atomic.sqlite");
    let store = Store::open(&path)?;
    let mut data = bundle("invalid")?;
    data.trace.outcome = Some(TraceOutcome::Success {
        result_hash: "ff".repeat(32),
    });
    store.persist_call_trace(&data)?;
    assert!(
        store
            .flush()
            .unwrap_err()
            .to_string()
            .contains("missing blob")
    );
    let connection = rusqlite::Connection::open(&path)?;
    assert_eq!(
        connection.query_row("SELECT count(*) FROM definition_records", [], |row| row
            .get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        connection.query_row("SELECT count(*) FROM cas", [], |row| row.get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        connection.query_row("SELECT count(*) FROM call_traces", [], |row| row
            .get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[test]
fn observed_operations_keep_exact_definition_ownership_after_rebuild() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("observations.sqlite");
    let store = Store::open(&path)?;
    let mut data = bundle("observed")?;
    data.observations = vec![
        loom_proto::TraceObservation {
            definition_hash: "11".repeat(32),
            op: "fs.list".into(),
        },
        loom_proto::TraceObservation {
            definition_hash: "22".repeat(32),
            op: "fs.read".into(),
        },
    ];
    store.persist_call_trace(&data)?;
    store.flush()?;
    assert_eq!(
        store.load_call_trace("observed")?.unwrap().observations,
        data.observations
    );
    drop(store);
    let store = Store::open(&path)?;
    store.rebuild_views()?;
    store.with_connection(|connection| {
        let root: String = connection.query_row(
            "SELECT op FROM def_effects WHERE def_hash=?",
            ["11".repeat(32)],
            |row| row.get(0),
        )?;
        let child: String = connection.query_row(
            "SELECT op FROM def_effects WHERE def_hash=?",
            ["22".repeat(32)],
            |row| row.get(0),
        )?;
        assert_eq!(root, "fs.list");
        assert_eq!(child, "fs.read");
        Ok(())
    })?;
    Ok(())
}
