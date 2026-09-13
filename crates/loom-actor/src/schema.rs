// One definition for fresh schemas and opening copied/restored actor files.
macro_rules! mailbox_indexes {
    () => {
        "CREATE INDEX IF NOT EXISTS inbox_state_seq ON inbox(state,seq);
CREATE INDEX IF NOT EXISTS inbox_state_epoch_seq ON inbox(state,defer_epoch,seq);
CREATE INDEX IF NOT EXISTS outbox_delivered_seq_idx ON outbox(delivered,seq,idx);"
    };
}

pub(crate) const MAILBOX_INDEXES: &str = mailbox_indexes!();

/// Runtime tables; domain schemas belong to registered behaviors.
pub const SCHEMA: &str = concat!("
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE caps(cap_id INTEGER PRIMARY KEY,target TEXT NOT NULL,epoch TEXT NOT NULL,rights INTEGER NOT NULL,mac BLOB NOT NULL);
CREATE TABLE revoked(cap_id INTEGER PRIMARY KEY);
CREATE TABLE inbox(seq INTEGER PRIMARY KEY, key TEXT UNIQUE, sender TEXT, msg BLOB, received_at INTEGER, state TEXT NOT NULL DEFAULT 'pending', defer_epoch INTEGER NOT NULL DEFAULT -1, defer_count INTEGER NOT NULL DEFAULT 0);
CREATE TABLE effects(seq INTEGER, idx INTEGER, kind TEXT, request BLOB, result BLOB, PRIMARY KEY(seq, idx));
CREATE TABLE outbox(seq INTEGER, idx INTEGER, target TEXT, msg BLOB, delivered INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(seq, idx));
CREATE TABLE code_changes(seq INTEGER PRIMARY KEY, behavior_hash TEXT NOT NULL, parent_hash TEXT, author TEXT, rationale TEXT, schema_sql TEXT);
CREATE TABLE dead_letters(seq INTEGER PRIMARY KEY, msg BLOB, error TEXT, at INTEGER);
CREATE TABLE children(id TEXT PRIMARY KEY, spawned_seq INTEGER, behavior_hash TEXT, init BLOB NOT NULL, restart TEXT NOT NULL, shutdown TEXT NOT NULL, link INTEGER NOT NULL, monitor INTEGER NOT NULL DEFAULT 0, child_type TEXT NOT NULL DEFAULT '\"worker\"');
CREATE TABLE monitors(ref TEXT PRIMARY KEY, target TEXT);
CREATE TABLE monitored_by(ref TEXT PRIMARY KEY, watcher TEXT);
CREATE TABLE links(peer TEXT PRIMARY KEY);
CREATE TABLE restarts(child TEXT, at INTEGER);
CREATE TABLE timers(ref TEXT PRIMARY KEY,target TEXT,msg BLOB,deadline INTEGER,kind TEXT,armed INTEGER NOT NULL DEFAULT 0,initiator TEXT);
CREATE TABLE calls(ref TEXT PRIMARY KEY,target TEXT,timer_ref TEXT);
CREATE TABLE shutdowns(child TEXT PRIMARY KEY,request TEXT NOT NULL);
CREATE TABLE snapshots(seq INTEGER PRIMARY KEY, path TEXT);
", mailbox_indexes!());

pub(crate) const SYSTEM_TABLES: &[&str] = &[
    "meta",
    "caps",
    "revoked",
    "inbox",
    "effects",
    "outbox",
    "code_changes",
    "dead_letters",
    "children",
    "snapshots",
    "monitors",
    "monitored_by",
    "links",
    "timers",
    "calls",
    "shutdowns",
];
