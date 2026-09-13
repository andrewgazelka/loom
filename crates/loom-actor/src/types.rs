//! Public configuration, lifecycle, and validation data.
use crate::ActorId;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Config {
    pub store: Option<crate::StoreConfig>,
    pub ship_interval: Duration,
    pub lease_ttl: Duration,
    pub lease_clock: std::sync::Arc<dyn crate::Clock>,
    pub io: Io,
    pub snapshot_every: i64,
    /// Retries after the first attempt, before supervision receives the error.
    pub max_retries: usize,
    pub retry_backoff: Duration,
    /// Messages one scheduler step may handle under the actor's connection lock
    /// before it pumps and yields; other callers on that actor wait at most this many
    /// transactions. Must be positive.
    pub batch_limit: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            store: None,
            ship_interval: Duration::from_secs(1),
            lease_ttl: Duration::from_secs(10),
            lease_clock: std::sync::Arc::new(crate::SystemClock),
            io: Io::Auto,
            snapshot_every: 64,
            max_retries: 3,
            retry_backoff: Duration::from_millis(10),
            batch_limit: 64,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    #[default]
    Local,
    Remote,
}
impl Durability {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Leaves through a trap's park/stop strategy or `Node::stop`.
    Running,
    /// Leaves through `Node::promote`, `Node::restart`, `Node::skip`, or `Node::stop`.
    Parked,
    /// Leaves only through explicit `Node::restart` (resume, skip, or reset).
    Stopped,
    /// Isolated replay; only `Node::stop` leaves this state. Promote preserves it.
    Fork,
}

impl Status {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "running" => Ok(Self::Running),
            "parked" => Ok(Self::Parked),
            "stopped" => Ok(Self::Stopped),
            "fork" => Ok(Self::Fork),
            _ => anyhow::bail!("unknown actor status {value:?}"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Trap {
    pub message: String,
    pub(crate) runtime: bool,
    pub(crate) durability: bool,
}

impl Trap {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), runtime: false, durability: false }
    }

    pub(crate) fn finish(saved: Option<Self>, result: Result<(), Self>) -> Result<(), Self> {
        match saved {
            Some(error) if !error.runtime => Err(error),
            Some(error) => match result {
                Err(returned) if !returned.runtime => Err(returned),
                _ => Err(error),
            },
            None => result,
        }
    }
}

/// Fully drained results: even an ignored SQL result has executed to completion.
#[derive(Debug)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<turso::Row>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TableHash {
    pub name: String,
    pub hash: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TableDifference {
    pub name: String,
    pub original_hash: String,
    pub fork_hash: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Verdict {
    Matched { tables: Vec<TableHash> },
    DivergedAt { seq: i64, idx: i64, expected: Vec<u8>, got: Vec<u8> },
    Trapped { seq: i64, error: String },
    Differs { tables: Vec<TableDifference> },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    #[default]
    Permanent,
    Transient,
    Temporary,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shutdown {
    #[default]
    Brutal,
    TimeoutMs(u64),
    Infinity,
}
fn linked() -> bool {
    true
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildType {
    #[default]
    Worker,
    Supervisor,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartVerb {
    Resume,
    Skip,
    Reset,
}
#[derive(Clone, Debug, Serialize)]
pub struct ChildSpec {
    pub durability: Durability,
    pub behavior_hash: String,
    pub init: Vec<u8>,
    pub restart: RestartPolicy,
    pub shutdown: Shutdown,
    pub link: bool,
    pub monitor: bool,
    #[serde(rename = "type")]
    pub child_type: ChildType,
}
impl<'de> Deserialize<'de> for ChildSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Fields {
            #[serde(default)]
            durability: Durability,
            behavior_hash: String,
            init: Vec<u8>,
            #[serde(default)]
            restart: RestartPolicy,
            shutdown: Option<Shutdown>,
            #[serde(default = "linked")]
            link: bool,
            #[serde(default)]
            monitor: bool,
            #[serde(default, rename = "type")]
            child_type: ChildType,
        }
        let fields = Fields::deserialize(deserializer)?;
        let shutdown = fields.shutdown.unwrap_or(match fields.child_type {
            ChildType::Supervisor => Shutdown::Infinity,
            ChildType::Worker => Shutdown::Brutal,
        });
        Ok(Self {
            durability: fields.durability,
            behavior_hash: fields.behavior_hash,
            init: fields.init,
            restart: fields.restart,
            shutdown,
            link: fields.link,
            monitor: fields.monitor,
            child_type: fields.child_type,
        })
    }
}
impl ChildSpec {
    pub fn new(hash: &str, init: &[u8], child_type: ChildType) -> Self {
        Self {
            durability: Durability::Local,
            behavior_hash: hash.into(),
            init: init.into(),
            restart: RestartPolicy::Permanent,
            shutdown: match child_type {
                ChildType::Supervisor => Shutdown::Infinity,
                ChildType::Worker => Shutdown::Brutal,
            },
            link: true,
            monitor: false,
            child_type,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChildState {
    pub id: ActorId,
    pub status: Status,
    pub behavior_hash: String,
    pub generation: i64,
    pub revision: i64,
    pub poison_revision: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TreeEntry {
    pub depth: usize,
    pub id: ActorId,
    pub status: Status,
    pub behavior_hash: String,
}

/// Database I/O selected explicitly for all files owned by the node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Io {
    #[default]
    Auto,
    Syscall,
    IoUring,
    Memory,
}
impl Io {
    pub(crate) fn name(self) -> Result<&'static str> {
        match self {
            Self::Auto => Ok(if cfg!(target_os = "linux") { "io_uring" } else { "syscall" }),
            Self::Syscall => Ok("syscall"),
            Self::Memory => Ok("memory"),
            Self::IoUring if cfg!(target_os = "linux") => Ok("io_uring"),
            Self::IoUring => anyhow::bail!("io_uring is unavailable on {}", std::env::consts::OS),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AssertionResult {
    pub query: String,
    pub passed: bool,
}
#[derive(Debug, Serialize)]
pub struct ValidationResult {
    pub verdict: Verdict,
    pub assertions: Vec<AssertionResult>,
}
