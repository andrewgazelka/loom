use anyhow::Result;
use loom_store::Store;
use serde_json::json;

#[test]
fn queued_results_are_visible_and_shutdown_drains() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("recording.sqlite");
    let descriptor;
    {
        let store = Store::open(&path)?;
        descriptor = store.enqueue_value("desc", &json!({"op":"fs.list"}))?;
        store
            .enqueue_recording(&json!({"type":"effect_invoked","def_hash":null,"op":"fs.list"}))?;
        store.enqueue_effect(&descriptor, "scope", 0, &json!([1, 2]))?;
        assert_eq!(
            store.effect_get(&descriptor, "scope", 0)?,
            Some(json!([1, 2]))
        );
        assert!(
            store
                .enqueue_effect(&descriptor, "scope", 0, &json!([3]))
                .is_err()
        );
    }
    let store = Store::open(path)?;
    assert_eq!(
        store.get_value::<serde_json::Value>(&descriptor)?,
        Some(json!({"op":"fs.list"}))
    );
    assert_eq!(
        store.effect_get(&descriptor, "scope", 0)?,
        Some(json!([1, 2]))
    );
    assert_eq!(store.events(None, 0, 100)?.len(), 2);
    Ok(())
}

#[test]
fn synchronous_log_mutation_follows_queued_recording() -> Result<()> {
    let store = Store::memory()?;
    store.enqueue_recording(&json!({"type":"effect_invoked","def_hash":null,"op":"first"}))?;
    let last = store.append("system", &json!({"type":"last"}), 0)?;
    let events = store.events(None, 0, 100)?;
    assert_eq!(events[0].event["op"], "first");
    assert_eq!(events[1].seq, last);
    Ok(())
}

#[test]
fn failed_writer_rolls_back_batch_and_stays_failed() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("failed.sqlite");
    let store = Store::open(&path)?;
    store.with_connection(|connection| {
        connection.execute_batch("CREATE TRIGGER reject_recording BEFORE INSERT ON effect_results BEGIN SELECT RAISE(ABORT, 'injected recording failure'); END;")?;
        Ok(())
    })?;
    store.enqueue_effect("desc", "scope", 0, &json!(1))?;
    let error = store.flush().unwrap_err().to_string();
    assert!(error.contains("injected recording failure"));
    assert!(store.flush().is_err());
    assert!(store.effect_get("desc", "scope", 0).is_err());
    assert!(store.enqueue_value("desc", &json!(1)).is_err());
    assert_eq!(store.recording_commit_count(), 0);
    let connection = rusqlite::Connection::open(path)?;
    let count: i64 =
        connection.query_row("SELECT count(*) FROM cas WHERE kind='result'", [], |row| {
            row.get(0)
        })?;
    assert_eq!(count, 0);
    Ok(())
}

#[test]
fn flush_makes_a_whole_batch_visible_to_another_connection() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("recording.sqlite");
    let store = Store::open(&path)?;
    for occurrence in 0..100 {
        store.enqueue_effect("desc", "scope", occurrence, &json!(occurrence))?;
    }
    store.flush()?;
    assert!((1..=3).contains(&store.recording_commit_count()));
    let connection = rusqlite::Connection::open(path)?;
    let count: i64 =
        connection.query_row("SELECT count(*) FROM effect_results", [], |row| row.get(0))?;
    assert_eq!(count, 100);
    Ok(())
}

#[test]
fn committed_conflicts_are_rejected_without_poisoning_writer() -> Result<()> {
    let store = Store::memory()?;
    store.enqueue_effect("desc", "scope", 0, &json!(1))?;
    store.flush()?;
    assert!(store.enqueue_effect("desc", "scope", 0, &json!(2)).is_err());
    assert_eq!(store.effect_get("desc", "scope", 0)?, Some(json!(1)));
    store.flush()?;
    Ok(())
}

#[test]
fn concurrent_duplicate_results_survive_barriers() -> Result<()> {
    let store = Store::memory()?;
    std::thread::scope(|scope| -> Result<()> {
        let mut workers = Vec::new();
        for _ in 0..4 {
            workers.push(scope.spawn(|| -> Result<()> {
                for occurrence in 0..100 {
                    store.enqueue_effect("desc", "scope", occurrence, &json!(occurrence))?;
                    assert_eq!(
                        store.effect_get("desc", "scope", occurrence)?,
                        Some(json!(occurrence))
                    );
                    if occurrence % 17 == 0 {
                        store.flush()?;
                    }
                }
                Ok(())
            }));
        }
        for worker in workers {
            worker.join().expect("recording worker panicked")?;
        }
        Ok(())
    })?;
    store.flush()?;
    assert_eq!(store.events(None, 0, 1000)?.len(), 100);
    Ok(())
}

#[test]
fn queued_encoding_matches_synchronous_cas_encoding() -> Result<()> {
    let store = Store::memory()?;
    let value = json!({"large":9_007_199_254_740_991_u64,"nested":[true,null,{"field":"value"}]});
    let queued = store.enqueue_value("desc", &value)?;
    let synchronous = store.put_value("desc", &value)?;
    assert_eq!(queued, synchronous);
    assert_eq!(store.get_value::<serde_json::Value>(&queued)?, Some(value));
    let invalid = json!({"large":u64::MAX});
    let queued_error = store
        .enqueue_value("desc", &invalid)
        .unwrap_err()
        .to_string();
    let synchronous_error = store.put_value("desc", &invalid).unwrap_err().to_string();
    assert_eq!(queued_error, synchronous_error);
    assert!(queued_error.contains("safe"));
    Ok(())
}

#[test]
fn blocked_checkpoint_is_not_reported_as_durable() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("busy.sqlite");
    let store = Store::open(&path)?;
    store.flush()?;
    store.with_connection(|connection| {
        connection.busy_timeout(std::time::Duration::ZERO)?;
        Ok(())
    })?;
    let reader = rusqlite::Connection::open(path)?;
    reader.execute_batch("BEGIN")?;
    let _: i64 = reader.query_row("SELECT count(*) FROM cas", [], |row| row.get(0))?;
    store.enqueue_effect("desc", "scope", 0, &json!(1))?;
    assert!(
        store
            .flush()
            .unwrap_err()
            .to_string()
            .contains("checkpoint is busy")
    );
    reader.execute_batch("ROLLBACK")?;
    assert!(store.flush().is_err());
    Ok(())
}

#[test]
fn queued_effect_hash_matches_cas_and_encoding_failure_leaves_no_pending_result() -> Result<()> {
    let store = Store::memory()?;
    let result = json!({"entries":["one","two"],"count":2});
    let hash = store.enqueue_effect("desc", "scope", 0, &result)?;
    assert_eq!(store.enqueue_effect("desc", "scope", 0, &result)?, hash);
    assert_eq!(store.effect_get("desc", "scope", 0)?, Some(result.clone()));
    store.flush()?;
    assert_eq!(store.enqueue_effect("desc", "scope", 0, &result)?, hash);
    assert_eq!(store.put_value("result", &result)?, hash);
    assert_eq!(store.get_value::<serde_json::Value>(&hash)?, Some(result));
    let invalid = json!({"large":u64::MAX});
    let queued_error = store
        .enqueue_effect("invalid", "scope", 0, &invalid)
        .unwrap_err()
        .to_string();
    let synchronous_error = store
        .effect_put("invalid", "scope", 0, &invalid)
        .unwrap_err()
        .to_string();
    assert_eq!(queued_error, synchronous_error);
    assert!(store.effect_get("invalid", "scope", 0)?.is_none());
    store.enqueue_effect("invalid", "scope", 0, &json!(1))?;
    store.flush()?;
    assert_eq!(store.effect_get("invalid", "scope", 0)?, Some(json!(1)));
    Ok(())
}
