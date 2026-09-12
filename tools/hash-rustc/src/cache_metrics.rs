//! Optional diagnostics. Cache entry points own their counters; the native backend
//! never calls them. Timings sum worker durations and are not additive wall time.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static HASHING_CALLS: AtomicU64 = AtomicU64::new(0);
static LOOKUP_CALLS: AtomicU64 = AtomicU64::new(0);
static STORE_CALLS: AtomicU64 = AtomicU64::new(0);
static PUBLISH_CALLS: AtomicU64 = AtomicU64::new(0);
static COPY_CALLS: AtomicU64 = AtomicU64::new(0);
static HASHING_NS: AtomicU64 = AtomicU64::new(0);
static LOOKUP_NS: AtomicU64 = AtomicU64::new(0);
static COPY_NS: AtomicU64 = AtomicU64::new(0);
static LLVM_NS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
pub enum Phase {
    Hashing,
    Lookup,
    ObjectCopy,
    Llvm,
}

pub struct Timer {
    start: Instant,
    phase: Phase,
}
impl Timer {
    pub fn start(phase: Phase) -> Self {
        Self {
            start: Instant::now(),
            phase,
        }
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        let counter = match self.phase {
            Phase::Hashing => &HASHING_NS,
            Phase::Lookup => &LOOKUP_NS,
            Phase::ObjectCopy => &COPY_NS,
            Phase::Llvm => &LLVM_NS,
        };
        counter.fetch_add(
            self.start
                .elapsed()
                .as_nanos()
                .try_into()
                .unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

pub fn hashing_call() {
    HASHING_CALLS.fetch_add(1, Ordering::Relaxed);
}
pub fn lookup_call() {
    LOOKUP_CALLS.fetch_add(1, Ordering::Relaxed);
}
pub fn store_call() {
    STORE_CALLS.fetch_add(1, Ordering::Relaxed);
}
pub fn publish_call() {
    PUBLISH_CALLS.fetch_add(1, Ordering::Relaxed);
}
pub fn copy_call() {
    COPY_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub fn print() {
    if std::env::var("LOOM_OBJECT_CACHE_CALLS").as_deref() == Ok("1") {
        eprintln!(
            "object-cache-calls: hashing={} lookup={} store={} publish={} object_copy={}",
            HASHING_CALLS.load(Ordering::Relaxed),
            LOOKUP_CALLS.load(Ordering::Relaxed),
            STORE_CALLS.load(Ordering::Relaxed),
            PUBLISH_CALLS.load(Ordering::Relaxed),
            COPY_CALLS.load(Ordering::Relaxed)
        );
    }
    if std::env::var("LOOM_OBJECT_CACHE_TIMINGS").as_deref() == Ok("1") {
        eprintln!(
            "object-cache-timing: hashing_ms={:.3} lookup_ms={:.3} object_copy_ms={:.3} llvm_ms={:.3}",
            HASHING_NS.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            LOOKUP_NS.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            COPY_NS.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            LLVM_NS.load(Ordering::Relaxed) as f64 / 1_000_000.0
        );
    }
}
