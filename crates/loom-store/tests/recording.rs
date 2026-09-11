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

#[test]
fn mixed_sync_and_queued_results_during_commits_never_poison_the_writer() -> Result<()> {
    use std::sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    let store = Store::memory()?;
    let start = Barrier::new(5);
    let completed = AtomicUsize::new(0);
    std::thread::scope(|scope| -> Result<()> {
        let mut workers = Vec::new();
        for worker in 0..4 {
            let start = &start;
            let completed = &completed;
            let store = &store;
            workers.push(scope.spawn(move || -> Result<()> {
                start.wait();
                let result = (|| -> Result<()> {
                    for occurrence in 0..200 {
                        let outcome = if worker == 0 {
                            store.effect_put("contention", "scope", occurrence, &json!(worker % 2))
                        } else {
                            store
                                .enqueue_effect("contention", "scope", occurrence, &json!(worker % 2))
                                .map(|_| ())
                        };
                        if let Err(error) = outcome {
                            anyhow::ensure!(
                                error.to_string() == "effect cache result conflict",
                                "unexpected enqueue failure: {error:#}"
                            );
                        }
                    }
                    Ok(())
                })();
                completed.fetch_add(1, Ordering::Release);
                result
            }));
        }
        start.wait();
        while completed.load(Ordering::Acquire) != 4 {
            store.flush()?;
            std::thread::yield_now();
        }
        for worker in workers {
            worker.join().expect("effect race worker panicked")?;
        }
        Ok(())
    })?;
    store.flush()?;
    assert!(store.events(None, 0, 1000)?.len() >= 200);
    for occurrence in 0..200 {
        let result = store.effect_get("contention", "scope", occurrence)?.unwrap();
        assert!(result == json!(0) || result == json!(1));
    }
    Ok(())
}

#[test]
fn weakened_durability_configuration_fails_closed() -> Result<()> {
    for pragma in ["PRAGMA synchronous=OFF", "PRAGMA journal_mode=DELETE"] {
        let directory = tempfile::tempdir()?;
        let store = Store::open(directory.path().join("config.sqlite"))?;
        let error = store
            .with_connection(|connection| {
                connection.execute_batch(pragma)?;
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("durable store requires"));
        assert!(store.flush().is_err());
        assert!(store.enqueue_effect("desc", "scope", 0, &json!(1)).is_err());
    }
    let ephemeral = Store::memory()?;
    ephemeral.with_connection(|connection| {
        let mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        assert_eq!(mode, "memory");
        Ok(())
    })?;
    ephemeral.enqueue_effect("desc", "scope", 0, &json!(1))?;
    ephemeral.flush()?;
    Ok(())
}

#[test]
fn failed_commit_is_sticky_and_rolls_back_results() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("commit.sqlite");
    let store = Store::open(&path)?;
    store.with_connection(|connection| {
        connection.execute_batch("CREATE TABLE commit_parent(id INTEGER PRIMARY KEY); CREATE TABLE commit_child(parent_id INTEGER REFERENCES commit_parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER fail_commit AFTER INSERT ON effect_results BEGIN INSERT INTO commit_child VALUES (1); END;")?;
        Ok(())
    })?;
    store.enqueue_effect("desc", "scope", 0, &json!(1))?;
    assert!(
        store
            .flush()
            .unwrap_err()
            .to_string()
            .contains("FOREIGN KEY constraint failed")
    );
    assert!(store.flush().is_err());
    assert_eq!(store.recording_commit_count(), 0);
    let reader = rusqlite::Connection::open(path)?;
    let count: i64 =
        reader.query_row("SELECT count(*) FROM effect_results", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    let count: i64 =
        reader.query_row("SELECT count(*) FROM cas WHERE kind='result'", [], |row| {
            row.get(0)
        })?;
    assert_eq!(count, 0);
    Ok(())
}

#[test]
fn flushed_wal_crash_child() -> Result<()> {
    let Some(path) = std::env::var_os("LOOM_RECORDING_CRASH_TEST_PATH") else {
        return Ok(());
    };
    let store = Store::open(path)?;
    let desc = store.enqueue_value("desc", &json!({"op":"crash-control"}))?;
    store.enqueue_effect(&desc, "crash-scope", 0, &json!({"answer":42}))?;
    store.flush()?;
    println!("LOOM_WAL_READY {desc}");
    std::io::Write::flush(&mut std::io::stdout())?;
    loop {
        std::thread::park();
    }
}

#[test]
fn acknowledged_result_survives_killed_process() -> Result<()> {
    use std::io::BufRead;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("crash.sqlite");
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "flushed_wal_crash_child", "--nocapture"])
        .env("LOOM_RECORDING_CRASH_TEST_PATH", &path)
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let mut output = std::io::BufReader::new(stdout);
    let desc = loop {
        let mut line = String::new();
        anyhow::ensure!(
            output.read_line(&mut line)? != 0,
            "crash child exited before acknowledgement"
        );
        if let Some(start) = line.find("LOOM_WAL_READY ") {
            break line[start + "LOOM_WAL_READY ".len()..].trim().to_owned();
        }
    };
    child.kill()?;
    let status = child.wait()?;
    assert!(!status.success());
    assert!(std::fs::metadata(path.with_extension("sqlite-wal"))?.len() > 0);
    // Recover using the primary file and WAL only, with no inherited shared index.
    let recovered = directory.path().join("recovered.sqlite");
    std::fs::copy(&path, &recovered)?;
    std::fs::copy(
        path.with_extension("sqlite-wal"),
        recovered.with_extension("sqlite-wal"),
    )?;
    let store = Store::open(recovered)?;
    assert_eq!(
        store.effect_get(&desc, "crash-scope", 0)?,
        Some(json!({"answer":42}))
    );
    assert_eq!(
        store.get_value::<serde_json::Value>(&desc)?,
        Some(json!({"op":"crash-control"}))
    );
    assert_eq!(store.events(None, 0, 100)?.len(), 1);
    Ok(())
}

#[test]
fn recording_timings_distinguish_transactions_from_empty_flushes() -> Result<()> {
    let store = Store::memory()?;
    let before = store.recording_timings();
    store.enqueue_effect("timing", "scope", 0, &json!(1))?;
    store.flush()?;
    let recorded = store.recording_timings();
    assert_eq!(
        recorded.committed_transactions - before.committed_transactions,
        1
    );
    assert!(recorded.transaction_nanos > before.transaction_nanos);
    assert_eq!(recorded.checkpoint_attempts - before.checkpoint_attempts, 1);
    assert!(recorded.checkpoint_nanos > before.checkpoint_nanos);
    store.flush()?;
    let empty = store.recording_timings();
    assert_eq!(
        empty.committed_transactions,
        recorded.committed_transactions
    );
    assert_eq!(empty.transaction_nanos, recorded.transaction_nanos);
    assert_eq!(empty.checkpoint_attempts, recorded.checkpoint_attempts + 1);
    assert!(empty.checkpoint_nanos > recorded.checkpoint_nanos);
    Ok(())
}
