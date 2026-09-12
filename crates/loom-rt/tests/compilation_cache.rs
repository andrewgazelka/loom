use loom_rt::LoomCompilationCache;
use loom_store::Store;
use std::{
    borrow::Cow,
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
use wasmtime::{CacheStore, Config, Engine, Instance, Module, OptLevel, Strategy};

#[derive(Clone, Debug, Default)]
struct CacheTraffic {
    lookups: usize,
    forced_misses: usize,
    candidate_hits: usize,
    hit_keys: BTreeSet<Vec<u8>>,
    inserted_keys: BTreeSet<Vec<u8>>,
    insert_failures: usize,
}

/// The engine configuration a cache is built for. The production cache namespaces
/// its rows by the engine that would replay them, so it and the engine under test
/// are built from the same configuration.
fn engine_config(opt_level: OptLevel) -> Config {
    let mut config = Config::new();
    config.strategy(Strategy::Cranelift);
    config.cranelift_opt_level(opt_level);
    config
}

#[derive(Debug)]
struct RecordingCache {
    production: LoomCompilationCache,
    opt_level: OptLevel,
    allowed_keys: Option<BTreeSet<Vec<u8>>>,
    traffic: Mutex<CacheTraffic>,
}

impl RecordingCache {
    fn new(store: Store, allowed_keys: Option<BTreeSet<Vec<u8>>>, opt_level: OptLevel) -> Self {
        Self {
            production: LoomCompilationCache::new(store, &engine_config(opt_level)).unwrap(),
            opt_level,
            allowed_keys,
            traffic: Mutex::new(CacheTraffic::default()),
        }
    }

    fn snapshot(&self) -> CacheTraffic {
        self.traffic.lock().unwrap().clone()
    }

    fn assert_no_errors(&self) {
        let stats = self.production.stats();
        assert_eq!(stats.errors, 0, "{:?}", stats.last_error);
        assert_eq!(stats.storage_errors, 0);
        assert_eq!(stats.missing_blobs, 0);
        assert_eq!(stats.corrupt_blobs, 0);
        assert_eq!(stats.conflicts, 0);
        assert_eq!(self.snapshot().insert_failures, 0);
    }
}

impl CacheStore for RecordingCache {
    fn get(&self, key: &[u8]) -> Option<Cow<'_, [u8]>> {
        {
            let mut traffic = self.traffic.lock().unwrap();
            traffic.lookups += 1;
            if self
                .allowed_keys
                .as_ref()
                .is_some_and(|keys| !keys.contains(key))
            {
                traffic.forced_misses += 1;
                return None;
            }
        }
        let result = self.production.get(key);
        if result.is_some() {
            let mut traffic = self.traffic.lock().unwrap();
            traffic.candidate_hits += 1;
            traffic.hit_keys.insert(key.to_vec());
        }
        result
    }

    fn insert(&self, key: &[u8], value: Vec<u8>) -> bool {
        let accepted = self.production.insert(key, value);
        let mut traffic = self.traffic.lock().unwrap();
        traffic.inserted_keys.insert(key.to_vec());
        if !accepted {
            traffic.insert_failures += 1;
        }
        accepted
    }
}

struct CompilerLog {
    messages: Mutex<Vec<String>>,
}

static COMPILER_LOG: CompilerLog = CompilerLog {
    messages: Mutex::new(Vec::new()),
};

impl log::Log for CompilerLog {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.target().contains("cranelift")
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let message = record.args().to_string();
            if message.starts_with("Incremental compilation cache stats: ") {
                self.messages.lock().unwrap().push(message);
            }
        }
    }

    fn flush(&self) {}
}

#[derive(Default)]
struct AcceptedCompilation {
    hits: usize,
    lookups: usize,
    reports: usize,
}

impl CompilerLog {
    fn collect(&self) -> AcceptedCompilation {
        let mut accepted = AcceptedCompilation::default();
        for message in std::mem::take(&mut *self.messages.lock().unwrap()) {
            let counts = message
                .strip_prefix("Incremental compilation cache stats: ")
                .unwrap()
                .split_once(' ')
                .unwrap()
                .0;
            let counts: Vec<&str> = counts.split('/').collect();
            assert_eq!(counts.len(), 2, "unexpected compiler report: {message}");
            accepted.hits += counts[0].parse::<usize>().unwrap();
            accepted.lookups += counts[1].parse::<usize>().unwrap();
            accepted.reports += 1;
        }
        accepted
    }
}

fn wasm(constant: i32, marker: u8) -> Vec<u8> {
    let mut bytes = wat::parse_str(format!(
        "(module (func (export \"f\") (param i32) (result i32) \
         local.get 0 i32.const {constant} i32.add))"
    ))
    .unwrap();
    // A valid custom section after code: one-byte name 'x', one payload byte.
    bytes.extend_from_slice(&[0, 3, 1, b'x', marker]);
    bytes
}

fn compile_and_invoke(
    cache: &Arc<RecordingCache>,
    wasm: &[u8],
    expected: i32,
) -> AcceptedCompilation {
    assert_eq!(COMPILER_LOG.collect().reports, 0, "unclaimed compiler log");
    {
        let mut config = engine_config(cache.opt_level);
        config
            .enable_incremental_compilation(cache.clone())
            .unwrap();
        // Whole-module caching is deliberately unconfigured.
        let engine = Engine::new(&config).unwrap();
        let module = Module::new(&engine, wasm).unwrap();
        let mut store = wasmtime::Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let function = instance
            .get_typed_func::<i32, i32>(&mut store, "f")
            .unwrap();
        assert_eq!(function.call(&mut store, 5).unwrap(), expected);
    }
    // Compiler statistics are emitted only after all engine owners are gone.
    let accepted = COMPILER_LOG.collect();
    assert!(
        accepted.reports > 0,
        "compiler did not emit its accepted-hit count"
    );
    assert!(
        accepted.lookups > 0,
        "incremental compilation was not exercised"
    );
    cache.assert_no_errors();
    let traffic = cache.snapshot();
    let stats = cache.production.stats();
    eprintln!(
        "lookups={} candidate_hits={} accepted_compiler_hits={} compiler_lookups={} \
         inserts={} forced_misses={} storage_errors={}",
        traffic.lookups,
        traffic.candidate_hits,
        accepted.hits,
        accepted.lookups,
        traffic.inserted_keys.len(),
        traffic.forced_misses,
        stats.storage_errors,
    );
    accepted
}

#[test]
fn shared_function_hits_cas_across_modules() {
    log::set_logger(&COMPILER_LOG).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("shared.sqlite");
    let module_a = wasm(7, b'A');
    let module_b = wasm(7, b'B');
    let module_c = wasm(8, b'C');
    assert_ne!(blake3::hash(&module_a), blake3::hash(&module_b));

    let cache_a = Arc::new(RecordingCache::new(
        Store::open(&database).unwrap(),
        None,
        OptLevel::Speed,
    ));
    let accepted_a = compile_and_invoke(&cache_a, &module_a, 12);
    let traffic_a = cache_a.snapshot();
    assert_eq!(accepted_a.hits, 0, "empty A cache unexpectedly hit");
    assert_eq!(traffic_a.candidate_hits, 0);
    assert!(!traffic_a.inserted_keys.is_empty());
    drop(cache_a);

    let cache_c = Arc::new(RecordingCache::new(
        Store::open(directory.path().join("control.sqlite")).unwrap(),
        None,
        OptLevel::Speed,
    ));
    let accepted_c = compile_and_invoke(&cache_c, &module_c, 13);
    let traffic_c = cache_c.snapshot();
    assert_eq!(accepted_c.hits, 0, "empty C cache unexpectedly hit");
    assert_eq!(traffic_c.candidate_hits, 0);
    assert!(!traffic_c.inserted_keys.is_empty());
    drop(cache_c);

    let body_keys: BTreeSet<Vec<u8>> = traffic_a
        .inserted_keys
        .difference(&traffic_c.inserted_keys)
        .cloned()
        .collect();
    assert!(
        !body_keys.is_empty(),
        "control did not distinguish a body key"
    );
    assert!(
        traffic_a
            .inserted_keys
            .intersection(&traffic_c.inserted_keys)
            .next()
            .is_some(),
        "control did not identify any shared ABI trampoline key"
    );

    // Reopen the persisted production store; only body keys can return bytes.
    let cache_b = Arc::new(RecordingCache::new(
        Store::open(&database).unwrap(),
        Some(body_keys.clone()),
        OptLevel::Speed,
    ));
    let accepted_b = compile_and_invoke(&cache_b, &module_b, 12);
    let traffic_b = cache_b.snapshot();
    assert!(accepted_b.hits > 0, "compiler rejected every cached body");
    assert!(
        !traffic_b.hit_keys.is_empty(),
        "B did not load a body from CAS"
    );
    assert!(traffic_b.hit_keys.is_subset(&body_keys));
    assert!(
        traffic_b.inserted_keys.is_disjoint(&body_keys),
        "B recompiled a cached body"
    );
    assert!(
        traffic_b.forced_misses > 0,
        "trampoline exclusion was not exercised"
    );
    assert_eq!(
        cache_b.production.stats().hits as usize,
        traffic_b.candidate_hits
    );
    eprintln!(
        "body_keys={} accepted_body_hits={}",
        body_keys.len(),
        accepted_b.hits
    );
    drop(cache_b);

    // The same persisted entries must miss when compiler flags change.
    let cache_flags = Arc::new(RecordingCache::new(
        Store::open(&database).unwrap(),
        None,
        OptLevel::None,
    ));
    let accepted_flags = compile_and_invoke(&cache_flags, &module_a, 12);
    let traffic_flags = cache_flags.snapshot();
    assert_eq!(accepted_flags.hits, 0, "changed compiler flags reused code");
    assert_eq!(traffic_flags.candidate_hits, 0);
    assert_eq!(traffic_flags.forced_misses, 0);
    assert!(!traffic_flags.inserted_keys.is_empty());
    assert!(
        traffic_flags
            .inserted_keys
            .is_disjoint(&traffic_a.inserted_keys)
    );
    eprintln!("changed_flags_candidate_hits=0 changed_flags_accepted_hits=0");
}
