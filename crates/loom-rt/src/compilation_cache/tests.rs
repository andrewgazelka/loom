use super::*;

#[test]
fn cache_conflicts_preserve_the_first_mapping_and_clear_releases_roots() -> Result<()> {
    let store = Store::memory()?;
    let cache = LoomCompilationCache::new(store.clone(), &Config::new())?;
    assert!(cache.get(b"function").is_none());
    assert_eq!(cache.stats().misses, 1);
    assert!(cache.insert(b"function", b"native code".to_vec()));
    assert!(cache.insert(b"function", b"native code".to_vec()));
    let hash = store.put("cranelift-function", b"native code")?;
    assert!(
        store
            .with_connection(|connection| {
                connection.execute("DELETE FROM cas WHERE hash=?", [&hash])?;
                Ok(())
            })
            .is_err(),
        "a live cache mapping must retain its CAS blob"
    );
    assert!(!cache.insert(b"function", b"different code".to_vec()));
    assert_eq!(cache.stats().conflicts, 1);
    assert_eq!(cache.get(b"function").unwrap().as_ref(), b"native code");
    assert_eq!(cache.clear()?, 1);
    store.with_connection(|connection| {
        connection.execute("DELETE FROM cas WHERE hash=?", [&hash])?;
        Ok(())
    })?;
    assert!(cache.get(b"function").is_none());
    Ok(())
}

#[test]
fn corrupt_and_missing_blobs_have_distinct_diagnostics() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("cache.sqlite");
    let store = Store::open(&path)?;
    let cache = LoomCompilationCache::new(store.clone(), &Config::new())?;
    assert!(cache.insert(b"function", b"native code".to_vec()));
    let hash = store.put("cranelift-function", b"native code")?;
    store.with_connection(|connection| {
        connection.execute(
            "UPDATE cas SET bytes=? WHERE hash=?",
            params![b"corrupt", hash],
        )?;
        Ok(())
    })?;
    assert!(cache.get(b"function").is_none());
    assert_eq!(cache.stats().corrupt_blobs, 1);
    assert!(cache.stats().last_error.unwrap().contains(&hash));
    assert!(!cache.insert(b"function", b"native code".to_vec()));
    assert_eq!(
        cache.stats().corrupt_blobs,
        2,
        "insert must not conceal corruption"
    );

    // Simulate an externally damaged database, bypassing its foreign-key guard.
    let external = rusqlite::Connection::open(path)?;
    external.pragma_update(None, "foreign_keys", false)?;
    assert!(!external.pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))?);
    external.execute("DELETE FROM cas WHERE hash=?", [&hash])?;
    assert!(cache.get(b"function").is_none());
    assert_eq!(cache.stats().missing_blobs, 1);
    assert!(cache.stats().last_error.unwrap().contains(&hash));
    Ok(())
}

#[test]
fn storage_failure_aborts_its_compile_window_and_retry_can_recover() -> Result<()> {
    let store = Store::memory()?;
    let cache = LoomCompilationCache::new(store.clone(), &Config::new())?;
    store.with_connection(|connection| {
        connection.execute_batch(
            "CREATE TRIGGER reject_cache_insert BEFORE INSERT ON runtime_compilation_cache
             BEGIN SELECT RAISE(FAIL, 'cache fixture unavailable'); END",
        )?;
        Ok(())
    })?;
    let result = cache.compile(|| {
        assert!(!cache.insert(b"function", b"native code".to_vec()));
        Ok(())
    });
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("cache fixture unavailable")
    );
    assert_eq!(cache.stats().storage_errors, 1);
    store.with_connection(|connection| {
        connection.execute_batch("DROP TRIGGER reject_cache_insert")?;
        Ok(())
    })?;
    cache.compile(|| {
        assert!(cache.insert(b"function", b"native code".to_vec()));
        Ok(())
    })?;
    assert_eq!(cache.get(b"function").unwrap().as_ref(), b"native code");
    Ok(())
}
