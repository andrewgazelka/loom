//! The result cache's copy on disk, so a restart does not start cold.
//!
//! The file is `cache.db` beside the store's database (SQLite, WAL,
//! `auto_vacuum=INCREMENTAL`). It is disposable: the in-memory index in the parent
//! module stays the authority for what is kept and what is evicted, and this file
//! only remembers it. Losing any of it, or all of it, costs recomputation and
//! nothing else, which shapes every choice below.
//!
//! * **Write-behind.** The caller's thread never touches SQLite. A store, a hit and
//!   an eviction are messages to one writer thread over a bounded channel; when the
//!   channel is full, or the values queued or being written pass 64 MiB, the
//!   message is dropped (a lost store is a future miss, a lost hit bump is a
//!   slightly stale priority, a lost delete leaves a row that the next load trims).
//!   Clearing is the exception: it waits for room, and if its transaction fails the
//!   writer keeps it and retries it ahead of the next batch, because a result that
//!   was cleared on purpose must not come back after a restart.
//! * **Rows belong to one host build.** Each row carries the host identity of the
//!   runtime that wrote it and a BLAKE3 checksum of its value. Load deletes the
//!   rows of any other identity before ranking (they could never be hit, and must
//!   not crowd out fresh ones) and the rows whose value does not match its
//!   checksum.
//! * **Batched hits.** Hit bumps are summed in the writer and written in one
//!   transaction per 1000 bumps or 5 seconds, whichever comes first. Stores and
//!   deletes share the transaction of the batch they arrive in.
//! * **Values above 1 MiB are memory-only.** Bigger results are rare, dear to write
//!   and slow to read back, and the store keeps its own large blobs as files; the
//!   in-memory cache still serves them until a restart.
//! * **Eager load.** Startup reads the rows worth most per byte (`hits * cost /
//!   size`, the same proxy the in-memory priority uses) into memory up to the byte
//!   cap and deletes the rest. Loading everything is bounded by the cache's own
//!   128 MiB and keeps `get` free of disk reads; a lazy `OnDisk` entry would save
//!   start-up time at the price of a second entry state on the hit path.
//! * **Never fatal.** A file that is corrupt (SQLite says corrupt or not a
//!   database) or of another format version is deleted and replaced by an empty
//!   one, with one warning on stderr. Any other failure (locked, permissions, disk
//!   full, I/O) leaves the file alone, since another runtime may have it open, and
//!   the cache runs without persistence.
use anyhow::{Context, Result};
use rusqlite::{Connection, Transaction, params};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

pub(super) type Digest = [u8; 32];

/// Largest value kept on disk; see the module comment.
pub(super) const MAX_VALUE_BYTES: usize = 1 << 20;

const FILE: &str = "cache.db";
/// Stored in `PRAGMA user_version`. A different value means an older or newer
/// layout, and the file is replaced.
const FORMAT_VERSION: i64 = 2;
/// Messages waiting for the writer before new ones are dropped.
const QUEUE: usize = 4096;
/// Bytes of values queued or being written before new stores are dropped.
const MAX_QUEUED_BYTES: usize = 64 << 20;
/// Messages applied in one transaction, at most.
const BATCH: usize = 512;
const HIT_BATCH: u64 = 1000;
const HIT_INTERVAL: Duration = Duration::from_secs(5);
/// How long `Drop` waits for the writer to flush before giving up on it.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
const VACUUM_PAGES: u32 = 256;

/// A count of bytes that may not pass a limit: stores reserve their value's size
/// before they are queued and the writer releases it once they are written.
pub(super) struct ByteBudget {
    used: AtomicUsize,
    max: usize,
}

impl ByteBudget {
    pub(super) fn new(max: usize) -> Self {
        Self {
            used: AtomicUsize::new(0),
            max,
        }
    }

    /// Take `bytes` if they fit under the limit.
    pub(super) fn reserve(&self, bytes: usize) -> bool {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|total| *total <= self.max)
            })
            .is_ok()
    }

    pub(super) fn release(&self, bytes: usize) {
        self.used.fetch_sub(bytes, Ordering::AcqRel);
    }

    pub(super) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

/// One result to remember.
pub(super) struct Row {
    pub(super) digest: Digest,
    pub(super) callee: String,
    pub(super) entry: String,
    pub(super) argc: u32,
    pub(super) cost_ns: u64,
    pub(super) hits: u64,
    pub(super) value: Arc<[u8]>,
}

/// One result read back at startup.
pub(super) struct Loaded {
    pub(super) digest: Digest,
    pub(super) callee: String,
    pub(super) cost_ns: u64,
    pub(super) hits: u64,
    pub(super) value: Vec<u8>,
}

enum Msg {
    Put(Row),
    Hit(Digest),
    Delete(Digest),
    Clear(Option<String>),
    Flush(mpsc::Sender<()>),
    Shutdown,
}

impl Msg {
    /// Bytes of value this message keeps alive while it waits.
    fn weight(&self) -> usize {
        match self {
            Msg::Put(row) => row.value.len(),
            _ => 0,
        }
    }
}

/// The handle the cache holds; dropping it flushes and stops the writer.
pub(super) struct Persistence {
    tx: SyncSender<Msg>,
    entries: Arc<AtomicU64>,
    /// Bytes of values queued or being written.
    budget: Arc<ByteBudget>,
    /// The writer thread and the channel that says it finished. A mutex only so
    /// the handle is `Sync`; `Drop` has it to itself.
    writer: Mutex<Option<(thread::JoinHandle<()>, mpsc::Receiver<()>)>>,
}

/// Open (or replace) `cache.db` in `dir` for the host build `identity`. Returns the
/// handle and the results to put back in memory, at most `max_bytes` of them, all
/// written by that build. `None` means run without persistence.
pub(super) fn open(
    dir: &Path,
    max_bytes: usize,
    identity: &Digest,
) -> Option<(Persistence, Vec<Loaded>)> {
    let path = dir.join(FILE);
    let (connection, loaded) = match try_open(&path, max_bytes, identity) {
        Ok(opened) => opened,
        Err(error) if is_locked(&error) => {
            eprintln!(
                "loom: result cache {} is locked ({error:#}); running without persistence",
                path.display()
            );
            return None;
        }
        Err(first) if is_corrupt(&first) => {
            remove_files(&path);
            match try_open(&path, max_bytes, identity) {
                Ok(opened) => {
                    eprintln!(
                        "loom: result cache {} was unusable ({first:#}); replaced it with an empty one",
                        path.display()
                    );
                    opened
                }
                Err(second) => {
                    eprintln!(
                        "loom: result cache {} is unusable ({first:#}; then {second:#}); running without persistence",
                        path.display()
                    );
                    return None;
                }
            }
        }
        Err(error) => {
            eprintln!(
                "loom: result cache {} cannot be used ({error:#}); left in place, running without persistence",
                path.display()
            );
            return None;
        }
    };
    let entries = Arc::new(AtomicU64::new(loaded.len() as u64));
    let budget = Arc::new(ByteBudget::new(MAX_QUEUED_BYTES));
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let (done_tx, done_rx) = mpsc::channel();
    let mut writer = Writer {
        connection,
        identity: *identity,
        entries: entries.clone(),
        budget: budget.clone(),
        pending_clears: Vec::new(),
        hits: HashMap::new(),
        bumps: 0,
        last_flush: Instant::now(),
        warned: false,
    };
    let spawned = thread::Builder::new()
        .name("loom-result-cache".into())
        .spawn(move || {
            writer.run(&rx);
            let _ = done_tx.send(());
        });
    match spawned {
        Ok(handle) => Some((
            Persistence {
                tx,
                entries,
                budget,
                writer: Mutex::new(Some((handle, done_rx))),
            },
            loaded,
        )),
        Err(error) => {
            eprintln!(
                "loom: result cache writer thread failed to start ({error}); running without persistence"
            );
            None
        }
    }
}

/// A file on which this build read a format it does not write.
#[derive(Debug)]
struct FormatMismatch {
    found: i64,
}

impl std::fmt::Display for FormatMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "format version {}, this build reads {FORMAT_VERSION}",
            self.found
        )
    }
}

impl std::error::Error for FormatMismatch {}

/// Whether `error` says the file itself is bad (another format, or SQLite's
/// corrupt and not-a-database codes) and replacing it is the cure. Anything else
/// may be a condition of the moment, or a file someone else is using.
pub(super) fn is_corrupt(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<FormatMismatch>()
            || matches!(
                cause.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::SqliteFailure(failure, _))
                    if matches!(
                        failure.code,
                        rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                    )
            )
    })
}

fn is_locked(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(failure, _))
                if matches!(
                    failure.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    })
}

fn remove_files(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let _ = std::fs::remove_file(name);
    }
}

fn sql(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn try_open(path: &Path, max_bytes: usize, identity: &Digest) -> Result<(Connection, Vec<Loaded>)> {
    let mut connection = Connection::open(path).context("open")?;
    connection.busy_timeout(Duration::from_secs(5))?;
    // `auto_vacuum` only takes effect before the first table exists.
    connection
        .execute_batch(
            "PRAGMA auto_vacuum=INCREMENTAL;
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA journal_size_limit=16777216;",
        )
        .context("pragmas")?;
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("format version")?;
    if version != 0 && version != FORMAT_VERSION {
        return Err(FormatMismatch { found: version }.into());
    }
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS results(
                 key BLOB PRIMARY KEY,
                 identity BLOB NOT NULL,
                 callee TEXT NOT NULL,
                 entry TEXT NOT NULL,
                 argc INTEGER NOT NULL,
                 cost_ns INTEGER NOT NULL,
                 hits INTEGER NOT NULL,
                 size INTEGER NOT NULL,
                 checksum BLOB NOT NULL,
                 value BLOB NOT NULL
             );",
        )
        .context("schema")?;
    if version == 0 {
        connection.execute_batch(&format!("PRAGMA user_version={FORMAT_VERSION}"))?;
    }
    let loaded = load(&mut connection, max_bytes, identity).context("load")?;
    Ok((connection, loaded))
}

/// Delete the rows of any other host identity, then read back the rows worth most
/// per byte until `max_bytes` is used, and delete the rest (rows that no longer
/// fit, rows over the value limit, rows whose size or checksum disagrees with their
/// value). Metadata is read first so values are read only for the rows kept.
fn load(connection: &mut Connection, max_bytes: usize, identity: &Digest) -> Result<Vec<Loaded>> {
    // Before ranking: rows no build like this one can hit must not take a place
    // from rows it can.
    let foreign = connection.execute("DELETE FROM results WHERE identity != ?", [&identity[..]])?;
    struct Meta {
        key: Vec<u8>,
        callee: String,
        size: i64,
        cost_ns: i64,
        hits: i64,
    }
    let metas: Vec<Meta> = connection
        .prepare(
            "SELECT key, callee, size, cost_ns, hits FROM results
             ORDER BY CAST(hits AS REAL) * cost_ns / MAX(size, 1) DESC",
        )?
        .query_map([], |row| {
            Ok(Meta {
                key: row.get(0)?,
                callee: row.get(1)?,
                size: row.get(2)?,
                cost_ns: row.get(3)?,
                hits: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut used = 0usize;
    let mut loaded = Vec::new();
    let mut doomed: Vec<Vec<u8>> = Vec::new();
    {
        let mut read = connection.prepare("SELECT value, checksum FROM results WHERE key = ?")?;
        for meta in metas {
            let size = usize::try_from(meta.size).unwrap_or(usize::MAX);
            let digest = Digest::try_from(meta.key.as_slice()).ok();
            let fits = size <= MAX_VALUE_BYTES && used.saturating_add(size) <= max_bytes;
            let Some(digest) = digest.filter(|_| fits) else {
                doomed.push(meta.key);
                continue;
            };
            let (value, checksum): (Vec<u8>, Vec<u8>) =
                read.query_row([&meta.key], |row| Ok((row.get(0)?, row.get(1)?)))?;
            if value.len() != size || blake3::hash(&value).as_bytes()[..] != checksum[..] {
                doomed.push(meta.key);
                continue;
            }
            used += size;
            loaded.push(Loaded {
                digest,
                callee: meta.callee,
                cost_ns: u64::try_from(meta.cost_ns).unwrap_or(0),
                hits: u64::try_from(meta.hits).unwrap_or(0).max(1),
                value,
            });
        }
    }
    if !doomed.is_empty() || foreign > 0 {
        let tx = connection.transaction()?;
        {
            let mut delete = tx.prepare("DELETE FROM results WHERE key = ?")?;
            for key in &doomed {
                delete.execute([key])?;
            }
        }
        tx.commit()?;
        connection.execute_batch(&format!("PRAGMA incremental_vacuum({VACUUM_PAGES})"))?;
    }
    Ok(loaded)
}

impl Persistence {
    /// Remember a result. Dropped when the writer is behind, by count or by bytes.
    pub(super) fn put(&self, row: Row) {
        let bytes = row.value.len();
        if !self.budget.reserve(bytes) {
            return;
        }
        if self.tx.try_send(Msg::Put(row)).is_err() {
            self.budget.release(bytes);
        }
    }

    /// Bytes of values queued or being written.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn queued_bytes(&self) -> usize {
        self.budget.used()
    }

    /// Count a hit on a persisted result. Dropped when the writer is behind.
    pub(super) fn hit(&self, digest: Digest) {
        self.offer(Msg::Hit(digest));
    }

    /// Forget an evicted result. Dropped when the writer is behind; the next load
    /// trims what that leaves.
    pub(super) fn delete(&self, digest: Digest) {
        self.offer(Msg::Delete(digest));
    }

    /// Forget every persisted result of `callee`, or all. Waits for room.
    pub(super) fn clear(&self, callee: Option<&str>) {
        let _ = self.tx.send(Msg::Clear(callee.map(str::to_owned)));
    }

    /// Rows in the file as of the last transaction.
    pub(super) fn entries(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    /// Wait until everything sent so far is in the file.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn flush(&self) {
        let (ack, done) = mpsc::channel();
        if self.tx.send(Msg::Flush(ack)).is_ok() {
            let _ = done.recv_timeout(SHUTDOWN_WAIT);
        }
    }

    fn offer(&self, msg: Msg) {
        // Full or disconnected: the message is dropped, which is safe.
        let _ = self.tx.try_send(msg);
    }
}

impl Drop for Persistence {
    fn drop(&mut self) {
        let writer = self.writer.get_mut().ok().and_then(Option::take);
        let Some((handle, done)) = writer else {
            return;
        };
        if self.tx.send(Msg::Shutdown).is_err() {
            return;
        }
        match done.recv_timeout(SHUTDOWN_WAIT) {
            Ok(()) => {
                let _ = handle.join();
            }
            Err(_) => eprintln!(
                "loom: result cache writer did not finish within {SHUTDOWN_WAIT:?}; detaching it"
            ),
        }
    }
}

struct Writer {
    connection: Connection,
    /// The host identity written into every row.
    identity: Digest,
    entries: Arc<AtomicU64>,
    budget: Arc<ByteBudget>,
    /// Clears whose transaction failed, retried ahead of the next batch.
    pending_clears: Vec<Option<String>>,
    /// Hits not yet written, by result.
    hits: HashMap<Digest, u64>,
    /// Hit messages since the last time hits were written.
    bumps: u64,
    last_flush: Instant,
    warned: bool,
}

impl Writer {
    fn run(&mut self, rx: &mpsc::Receiver<Msg>) {
        loop {
            let wait = HIT_INTERVAL.saturating_sub(self.last_flush.elapsed());
            let mut batch = Vec::new();
            let mut closing = false;
            match rx.recv_timeout(wait) {
                Ok(msg) => batch.push(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => closing = true,
            }
            while batch.len() < BATCH {
                match rx.try_recv() {
                    Ok(msg) => batch.push(msg),
                    Err(_) => break,
                }
            }
            let mut acks = Vec::new();
            let mut ops = Vec::with_capacity(batch.len());
            for msg in batch {
                match msg {
                    Msg::Flush(ack) => acks.push(ack),
                    Msg::Shutdown => closing = true,
                    other => ops.push(other),
                }
            }
            let weight: usize = ops.iter().map(Msg::weight).sum();
            self.apply(ops, closing || !acks.is_empty());
            self.budget.release(weight);
            for ack in acks {
                let _ = ack.send(());
            }
            if closing {
                return;
            }
        }
    }

    fn apply(&mut self, arrived: Vec<Msg>, force: bool) {
        // A clear that failed earlier comes before anything that arrived since.
        let mut ops: Vec<Msg> = std::mem::take(&mut self.pending_clears)
            .into_iter()
            .map(Msg::Clear)
            .collect();
        ops.extend(arrived);
        let clears: Vec<Option<String>> = ops
            .iter()
            .filter_map(|op| match op {
                Msg::Clear(scope) => Some(scope.clone()),
                _ => None,
            })
            .collect();
        let timer = self.last_flush.elapsed() >= HIT_INTERVAL;
        let due = force || timer;
        if ops.is_empty() && !(due && !self.hits.is_empty()) {
            if timer {
                self.last_flush = Instant::now();
            }
            return;
        }
        let changes_rows = ops.iter().any(|op| !matches!(op, Msg::Hit(_)));
        let deletes = ops
            .iter()
            .any(|op| matches!(op, Msg::Delete(_) | Msg::Clear(_)));
        let result = self.transaction(ops, due);
        // Empty means the hits were written, or lost with a failed transaction.
        if self.hits.is_empty() {
            self.last_flush = Instant::now();
            self.bumps = 0;
        }
        match result {
            Ok(()) => {
                self.warned = false;
                if changes_rows {
                    self.count();
                }
                if deletes {
                    let _ = self
                        .connection
                        .execute_batch(&format!("PRAGMA incremental_vacuum({VACUUM_PAGES})"));
                }
            }
            Err(error) => {
                self.hits.clear();
                self.keep_for_retry(clears);
                if !self.warned {
                    self.warned = true;
                    eprintln!("loom: result cache write failed ({error:#}); entries may be lost");
                }
            }
        }
    }

    /// Remember clears whose transaction failed. A full clear covers the others.
    fn keep_for_retry(&mut self, clears: Vec<Option<String>>) {
        for scope in clears {
            if !self.pending_clears.contains(&scope) {
                self.pending_clears.push(scope);
            }
        }
        if self.pending_clears.contains(&None) {
            self.pending_clears.retain(Option::is_none);
        }
    }

    fn transaction(&mut self, ops: Vec<Msg>, write_hits_now: bool) -> Result<()> {
        // Fields are used separately so the open transaction can borrow the
        // connection while the hit map is updated.
        let Self {
            connection,
            identity,
            hits,
            bumps,
            ..
        } = self;
        let tx = connection.transaction()?;
        for op in ops {
            match op {
                Msg::Put(row) => {
                    hits.remove(&row.digest);
                    tx.prepare_cached(
                        "INSERT OR REPLACE INTO results(key, identity, callee, entry, argc, cost_ns, hits, size, checksum, value)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    )?
                    .execute(params![
                        &row.digest[..],
                        &identity[..],
                        row.callee,
                        row.entry,
                        row.argc,
                        sql(row.cost_ns),
                        sql(row.hits),
                        row.value.len() as i64,
                        &blake3::hash(&row.value).as_bytes()[..],
                        &row.value[..],
                    ])?;
                }
                Msg::Hit(digest) => {
                    *hits.entry(digest).or_default() += 1;
                    *bumps += 1;
                }
                Msg::Delete(digest) => {
                    hits.remove(&digest);
                    tx.prepare_cached("DELETE FROM results WHERE key = ?")?
                        .execute([&digest[..]])?;
                }
                Msg::Clear(callee) => {
                    write_hits(hits, &tx)?;
                    match callee {
                        Some(callee) => tx
                            .prepare_cached("DELETE FROM results WHERE callee = ?")?
                            .execute([callee])?,
                        None => tx.execute("DELETE FROM results", [])?,
                    };
                }
                Msg::Flush(_) | Msg::Shutdown => {}
            }
        }
        if write_hits_now || *bumps >= HIT_BATCH {
            write_hits(hits, &tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn count(&self) {
        if let Ok(count) = self
            .connection
            .query_row("SELECT count(*) FROM results", [], |row| {
                row.get::<_, i64>(0)
            })
        {
            self.entries.store(count.max(0) as u64, Ordering::Relaxed);
        }
    }
}

fn write_hits(hits: &mut HashMap<Digest, u64>, tx: &Transaction<'_>) -> Result<()> {
    let mut update = tx.prepare_cached("UPDATE results SET hits = hits + ? WHERE key = ?")?;
    for (digest, bumps) in hits.drain() {
        update.execute(params![sql(bumps), &digest[..]])?;
    }
    Ok(())
}
