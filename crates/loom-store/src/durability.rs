//! Conditional policy, not wired into Store: NORMAL WAL with background-owned
//! generation changes. Requires approval of rare request waits during rotation.
//!
//! A reader pins a nonzero WAL snapshot so foreground writes cannot reset its
//! header (SQLite walRestartLog). Maintenance excludes writers while releasing
//! that pin, checkpointing, priming and pinning the next generation. NORMAL's
//! header xSync remains real and background-owned; waiting writers may depend
//! on it. Counters below measure maintenance, NOT native xSync calls.
use anyhow::{Result, anyhow, ensure};
use rusqlite::Connection;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Default)]
struct State {
    failure: Mutex<Option<String>>,
    rotations: AtomicU64,
    deferred: AtomicU64,
    lock_wait_nanos: AtomicU64,
    lock_hold_nanos: AtomicU64,
}
#[derive(Clone, Default)]
pub(crate) struct Health {
    state: Arc<State>,
}
#[derive(Debug)]
pub(crate) struct MaintenanceTimings {
    pub rotations: u64,
    pub deferred: u64,
    pub lock_wait_nanos: u64,
    pub lock_hold_nanos: u64,
}
impl Health {
    /// Caller MUST hold the writer connection lock and check before every write.
    pub fn check(&self) -> Result<()> {
        let failure = self
            .state
            .failure
            .lock()
            .map_err(|_| anyhow!("WAL health poisoned"))?;
        if let Some(message) = failure.as_ref() {
            return Err(anyhow!("WAL maintenance failed: {message}"));
        }
        Ok(())
    }
    fn fail(&self, error: &anyhow::Error) {
        if let Ok(mut failure) = self.state.failure.lock() {
            *failure = Some(format!("{error:#}"));
        }
    }
    pub fn timings(&self) -> MaintenanceTimings {
        MaintenanceTimings {
            rotations: self.state.rotations.load(Ordering::Relaxed),
            deferred: self.state.deferred.load(Ordering::Relaxed),
            lock_wait_nanos: self.state.lock_wait_nanos.load(Ordering::Relaxed),
            lock_hold_nanos: self.state.lock_hold_nanos.load(Ordering::Relaxed),
        }
    }
}
enum Command {
    Rotate(mpsc::SyncSender<std::result::Result<(), String>>),
    Stop,
}
pub(crate) struct BackgroundDurability {
    health: Health,
    sender: mpsc::Sender<Command>,
    worker: Option<JoinHandle<()>>,
}
impl BackgroundDurability {
    /// Complete before admitting requests. Keep alive until recording drains.
    pub fn start(path: PathBuf, writer: Arc<Mutex<Connection>>) -> Result<Self> {
        let health = Health::default();
        let worker_health = health.clone();
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("loom-wal-maintenance".into())
            .spawn(move || {
                if let Err(error) = run(path, writer, &worker_health, receiver, ready_sender) {
                    worker_health.fail(&error);
                }
            })?;
        let owner = Self {
            health,
            sender,
            worker: Some(worker),
        };
        ready_receiver
            .recv()
            .map_err(|error| anyhow!("WAL initialization ended: {error}"))?
            .map_err(|error| anyhow!(error))?;
        Ok(owner)
    }
    pub fn health(&self) -> Health {
        self.health.clone()
    }
    /// Diagnostic witness hook: forces rollover on the named maintenance thread.
    /// Do not call from a request path. Observe native xSync separately.
    pub fn rotate_for_witness(&self) -> Result<()> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.sender.send(Command::Rotate(sender))?;
        receiver.recv()?.map_err(|error| anyhow!(error))
    }
}
impl Drop for BackgroundDurability {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                self.health.fail(&anyhow!("WAL maintenance panicked"));
            }
        }
    }
}
fn run(
    path: PathBuf,
    writer: Arc<Mutex<Connection>>,
    health: &Health,
    commands: mpsc::Receiver<Command>,
    ready: mpsc::SyncSender<std::result::Result<(), String>>,
) -> Result<()> {
    let initialization = (|| -> Result<Connection> {
        let connection = writer
            .lock()
            .map_err(|_| anyhow!("store connection poisoned"))?;
        let mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        let synchronous: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
        ensure!(
            mode == "wal" && synchronous == 1,
            "WAL maintenance requires NORMAL WAL"
        );
        connection.execute_batch("PRAGMA wal_autocheckpoint=0;
            CREATE TABLE IF NOT EXISTS loom_wal_generation (id INTEGER PRIMARY KEY CHECK(id=1), generation INTEGER NOT NULL);
            INSERT OR IGNORE INTO loom_wal_generation VALUES(1,0);")?;
        let reader = Connection::open(path)?;
        reader.busy_timeout(Duration::ZERO)?;
        prime(&connection, &reader)?;
        Ok(reader)
    })();
    let reader = match initialization {
        Ok(reader) => reader,
        Err(error) => {
            health.fail(&error);
            let _ = ready.send(Err(format!("{error:#}")));
            return Err(error);
        }
    };
    let mut previous_changes = writer
        .lock()
        .map_err(|_| anyhow!("store connection poisoned"))?
        .total_changes();
    let _ = ready.send(Ok(()));
    let mut retry_deferred = false;
    loop {
        let completion = match commands.recv_timeout(Duration::from_secs(1)) {
            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Command::Rotate(completion)) => Some(completion),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
        };
        let waiting = Instant::now();
        let connection = writer
            .lock()
            .map_err(|_| anyhow!("store connection poisoned"))?;
        health
            .state
            .lock_wait_nanos
            .fetch_add(nanos(waiting), Ordering::Relaxed);
        let holding = Instant::now();
        let deferred_before = health.state.deferred.load(Ordering::Relaxed);
        let result = if completion.is_some()
            || retry_deferred
            || connection.total_changes() != previous_changes
        {
            rotate(&connection, &reader, health)
        } else {
            Ok(())
        };
        retry_deferred = health.state.deferred.load(Ordering::Relaxed) != deferred_before;
        previous_changes = connection.total_changes();
        if let Err(error) = &result {
            health.fail(error);
        }
        health
            .state
            .lock_hold_nanos
            .fetch_add(nanos(holding), Ordering::Relaxed);
        drop(connection);
        if let Some(completion) = completion {
            let _ = completion.send(
                result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|error| format!("{error:#}")),
            );
        }
        result?;
    }
    Ok(())
}
fn rotate(writer: &Connection, reader: &Connection, health: &Health) -> Result<()> {
    reader.execute_batch("ROLLBACK")?;
    // An external reader can defer reclamation. Re-prime and re-pin before
    // releasing the mutex; ordinary requests never encounter an unpinned WAL.
    // Such readers can prevent bounded reclamation and must be reported.
    let busy_timeout_ms: u64 = writer.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
    writer.busy_timeout(Duration::ZERO)?;
    let checkpoint = writer.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        row.get::<_, i64>(0)
    });
    let restore = writer.busy_timeout(Duration::from_millis(busy_timeout_ms));
    let busy = checkpoint?;
    restore?;
    prime(writer, reader)?;
    if busy == 0 {
        health.state.rotations.fetch_add(1, Ordering::Relaxed);
    } else {
        health.state.deferred.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}
fn prime(writer: &Connection, reader: &Connection) -> Result<()> {
    writer.execute(
        "UPDATE loom_wal_generation SET generation=generation+1 WHERE id=1",
        [],
    )?;
    reader.execute_batch("BEGIN")?;
    let _: i64 = reader.query_row(
        "SELECT generation FROM loom_wal_generation WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    Ok(())
}
fn nanos(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_blocks_reset_and_background_rotation_restores_pin() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("store.db");
        let connection = Connection::open(&path)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
            CREATE TABLE values_under_test(value INTEGER);",
        )?;
        let writer = Arc::new(Mutex::new(connection));
        let owner = BackgroundDurability::start(path.clone(), writer.clone())?;
        let observer = Connection::open(path)?;
        observer.busy_timeout(Duration::ZERO)?;
        for value in 0..3 {
            {
                let connection = writer.lock().unwrap();
                owner.health().check()?;
                connection.execute("INSERT INTO values_under_test VALUES(?)", [value])?;
            }
            let busy: i64 =
                observer.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
            ensure!(busy == 1, "live pin must prevent a foreign WAL reset");
            owner.rotate_for_witness()?;
            let busy: i64 =
                observer.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
            ensure!(busy == 1, "rotation must restore its pin");
        }
        ensure!(
            owner.health().timings().rotations >= 3,
            "forced rotations completed"
        );
        drop(owner);
        let busy: i64 =
            observer.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        ensure!(
            busy == 0,
            "removing pin must permit reset: negative control"
        );
        let count: i64 =
            observer.query_row("SELECT count(*) FROM values_under_test", [], |row| {
                row.get(0)
            })?;
        ensure!(count == 3, "rotation retains committed rows");
        Ok(())
    }
}
