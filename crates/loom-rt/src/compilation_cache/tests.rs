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

/// A module with many exported functions, imports and indirect calls: enough to make wasmtime create
/// the per-function contexts whose leftovers used to change later functions' cache keys.
fn many_functions_wat() -> String {
    let mut wat = String::from(
        "(module (import \"env\" \"memory\" (memory 1 1 shared)) (import \"h\" \"a\" (func $a (param i32) (result i32)))\n\
         (type $t (func (param i32) (result i32))) (table 8 funcref)\n",
    );
    for i in 0..60 {
        let indirect = if i % 3 == 0 { "i32.const 1 call_indirect (type $t)" } else { "" };
        wat.push_str(&format!(
            "(func $f{i} (export \"f{i}\") (param i32) (result i32) local.get 0 i32.const {} i32.add call $a i32.const 3 i32.mul {indirect} i32.const {i} i32.xor)\n",
            i + 1
        ));
    }
    wat.push_str("(elem (i32.const 0) $f0 $f1 $f2))");
    wat
}

#[test]
fn recompiling_the_same_module_through_the_cache_stores_nothing_new() -> Result<()> {
    // wasmtime pools a Cranelift context per thread, and `DataFlowGraph::clear` used to forget
    // `exception_tables`, so a function's cache key depended on what the context had compiled before:
    // an identical recompile kept missing (25 of 124 lookups for this module). `vendor/README.md`.
    let store = Store::memory()?;
    let (engine, cache) = crate::sharedcore::engine(store)?;
    let wat = many_functions_wat();
    let first = cache.stats();
    let module = cache.compile(|| wasmtime::Module::new(&engine, &wat).map_err(|error| anyhow::anyhow!("{error:#}")))?;
    drop(module);
    let after_first = cache.stats();
    assert!(after_first.inserts > first.inserts, "the first compile fills the cache");
    for round in 1..=3 {
        let before = cache.stats();
        cache.compile(|| wasmtime::Module::new(&engine, &wat).map_err(|error| anyhow::anyhow!("{error:#}")))?;
        let after = cache.stats();
        assert_eq!(
            after.inserts, before.inserts,
            "recompile {round} of an identical module compiled {} functions again",
            after.inserts - before.inserts
        );
    }
    Ok(())
}
