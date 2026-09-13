mod inspection;
pub(crate) use inspection::{inspect_query, inspect_statement};
mod execution;
mod initialize;
use crate::{Behavior, Ctx, EffectHandler, Registry, Rows, Status, Trap};
use anyhow::{Context, Result, ensure};
pub(crate) use execution::{Attempt, AttemptState, attempt};
pub(crate) use initialize::initialize;
use std::{path::Path, sync::Arc};
use tokio::sync::Mutex;
use turso::{Connection, IntoParams};

#[derive(Clone)]
pub struct Actor {
    pub(crate) id: String,
    pub(crate) conn: Arc<Mutex<Connection>>,
    pub(crate) managed: bool,
    pub(crate) node: crate::Node,
}

pub(crate) async fn connect(path: &Path, io: crate::Io) -> Result<Connection> {
    let path = path.to_str().context("database path is not UTF-8")?;
    let db = turso::Builder::new_local(path).with_io(io.name()?.to_owned()).experimental_vacuum(true).build().await?;
    let conn = db.connect()?;
    query(&conn, "PRAGMA journal_mode=WAL", ()).await?;
    query(&conn, "PRAGMA synchronous=NORMAL", ()).await?;
    Ok(conn)
}

pub(crate) async fn query(conn: &Connection, sql: &str, params: impl IntoParams) -> Result<Rows> {
    let mut result = conn.query(sql, params).await?;
    let columns = result.column_names();
    let mut rows = Vec::new();
    while let Some(row) = result.next().await? {
        rows.push(row);
    }
    Ok(Rows { columns, rows })
}

pub(crate) async fn meta(conn: &Connection, key: &str) -> Result<String> {
    let result = query(conn, "SELECT value FROM meta WHERE key=?", [key]).await?;
    result.rows.first().context(format!("missing meta key {key}"))?.get::<String>(0).map_err(Into::into)
}

pub(crate) async fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute("INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key, value]).await?;
    Ok(())
}

pub(crate) async fn cursor(conn: &Connection) -> Result<i64> {
    Ok(meta(conn, "cursor").await?.parse()?)
}
pub(crate) async fn status(conn: &Connection) -> Result<Status> {
    Status::parse(&meta(conn, "status").await?)
}

pub(crate) struct Code {
    pub revision: i64,
    pub hash: String,
}
pub(crate) async fn code(conn: &Connection) -> Result<Code> {
    let rows = query(conn, "SELECT seq,behavior_hash FROM code_changes ORDER BY seq DESC LIMIT 1", ()).await?;
    let row = rows.rows.first().context("missing behavior revision")?;
    Ok(Code { revision: row.get(0)?, hash: row.get(1)? })
}

pub(crate) async fn behavior(registry: &Arc<dyn Registry>, hash: &str) -> Result<Arc<dyn Behavior>> {
    if hash == crate::supervisor::HASH {
        return Ok(Arc::new(crate::Supervisor));
    }
    registry.resolve(hash).await
}

pub(crate) struct Message {
    pub seq: i64,
    pub msg: Vec<u8>,
    pub sender: Option<String>,
}
pub(crate) async fn next(conn: &Connection) -> Result<Option<Message>> {
    crate::mailbox::next(conn).await
}

pub(crate) async fn inject(conn: &Connection, key: &str, sender: &str, msg: &[u8]) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO inbox(key,sender,msg,received_at) VALUES (?,?,?,?)",
        turso::params![key, sender, msg, crate::effects::now()?],
    )
    .await?;
    Ok(())
}

impl Actor {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub async fn sql(&self, sql: &str, params: impl IntoParams + Send) -> Result<Rows> {
        let conn = self.conn.lock().await;
        if self.managed {
            return inspect_query(&conn, sql, params).await;
        }
        let before = query(&conn, "SELECT total_changes()", ()).await?.rows[0].get::<i64>(0)?;
        let result = query(&conn, sql, params).await.with_context(|| format!("actor {} seq -1: SQL", self.id));
        let after = query(&conn, "SELECT total_changes()", ()).await?.rows[0].get::<i64>(0)?;
        if before != after {
            // Host SQL is a supported mutation path, including lifecycle fixture
            // barriers. Its actor gets the same wake as a committed runtime change.
            let requests = query(&conn, "SELECT child FROM shutdowns", ()).await?;
            {
                let mut state = self.node.scheduling()?;
                state.index_dirty.insert(self.id.clone());
                state.shutdown_requesters.retain(|_, owners| {
                    owners.remove(&self.id);
                    !owners.is_empty()
                });
                for row in requests.rows {
                    state.shutdown_requesters.entry(row.get::<String>(0)?).or_default().insert(self.id.clone());
                }
                state.shutdown_dirty.insert(self.id.clone());
            }
            self.node.wake_actor(&self.id)?;
            self.node.request_timer_scan()?;
        }
        result
    }
    /// Inspect exactly one SELECT; reject writes before any statement executes.
    pub async fn inspect_sql(&self, sql: &str, params: Vec<turso::Value>) -> Result<Rows> {
        inspect_query(&*self.conn.lock().await, sql, params).await.with_context(|| format!("actor {} seq -1: SQL", self.id))
    }
    pub async fn cursor(&self) -> Result<i64> {
        cursor(&*self.conn.lock().await).await.with_context(|| format!("actor {} seq -1: cursor", self.id))
    }
    pub async fn status(&self) -> Result<Status> {
        status(&*self.conn.lock().await).await.with_context(|| format!("actor {} seq -1: status", self.id))
    }
}

pub(crate) async fn poison(
    conn: &mut Connection,
    message: &Message,
    error: &str,
    behavior: &dyn Behavior,
    effects: &dyn EffectHandler,
    node: &crate::Node,
) -> Result<()> {
    let identity = meta(conn, "id").await?;
    let id = identity.as_str();
    let tx = conn.transaction().await?;
    tx.execute(
        "INSERT OR IGNORE INTO dead_letters(seq,msg,error,at) VALUES (?,?,?,?)",
        turso::params![message.seq, message.msg.as_slice(), error, crate::effects::now()?],
    )
    .await?;
    set_meta(&tx, "poison_revision", &code(&tx).await?.revision.to_string()).await?;
    set_meta(&tx, "poison_seq", &message.seq.to_string()).await?;
    let strategy = meta(&tx, "strategy").await?;
    let strategy =
        if matches!(strategy.as_str(), "one_for_one" | "one_for_all" | "rest_for_one" | "dynamic") { "park" } else { strategy.as_str() };
    match strategy {
        "park" => set_meta(&tx, "status", "parked").await?,
        "skip" => {
            crate::mailbox::complete(&tx, message.seq).await?;
            set_meta(&tx, &format!("skipped:{}", message.seq), "1").await?;
            set_meta(&tx, &format!("code_at:{}", message.seq), &code(&tx).await?.revision.to_string()).await?;
        }
        "stop" => {
            crate::hooks::terminate(&tx, id, "poison", behavior, effects).await?;
            tx.execute("DELETE FROM timers", ()).await?;
            set_meta(&tx, "status", "stopped").await?;
            set_meta(&tx, "reason", "poison").await?;
        }
        other => anyhow::bail!("unknown supervision strategy {other}"),
    }
    notify(&tx, id, message.seq, "poison", Some(error), strategy == "stop", None).await?;
    node.commit_control(id, tx).await?;
    if strategy == "stop" {
        node.close_drivers(Some(id), None).await?;
    }
    Ok(())
}

pub(crate) async fn promote(
    conn: &mut Connection,
    behavior: &dyn Behavior,
    author: &str,
    rationale: &str,
    effects: &dyn EffectHandler,
    node: Option<&crate::Node>,
) -> Result<()> {
    let tx = conn.transaction().await?;
    let status = status(&tx).await?;
    ensure!(status != Status::Stopped, "stopped actor cannot be promoted");
    let previous = code(&tx).await?;
    let seen = query(&tx, "SELECT seq FROM code_changes WHERE behavior_hash=? LIMIT 1", [behavior.hash()]).await?;
    if seen.rows.is_empty() {
        tx.execute_batch(behavior.schema()).await?;
    }
    crate::hooks::upgrade(&tx, &meta(&tx, "id").await?, behavior, &previous.hash, effects).await?;
    tx.execute(
        "INSERT INTO code_changes(seq,behavior_hash,parent_hash,author,rationale,schema_sql) VALUES (?,?,?,?,?,?)",
        turso::params![
            previous.revision.checked_add(1).context("code revision overflow")?,
            behavior.hash(),
            previous.hash,
            author,
            rationale,
            behavior.schema()
        ],
    )
    .await?;
    if status == Status::Parked {
        set_meta(&tx, "status", "running").await?;
    }
    if let Some(node) = node {
        node.commit_control(&meta(&tx, "id").await?, tx).await?;
    } else {
        tx.commit().await?;
    }
    Ok(())
}

pub(crate) async fn snapshot(conn: &Connection, path: &Path, seq: i64) -> Result<()> {
    let known = query(conn, "SELECT path FROM snapshots WHERE seq=?", [seq]).await?;
    if !known.rows.is_empty() {
        return Ok(());
    }
    let path = path.to_str().context("snapshot path is not UTF-8")?;
    let staging = format!("{path}.pending");
    // A previous crash can leave this unreferenced staging copy.
    if Path::new(&staging).exists() {
        std::fs::remove_file(&staging)?;
    }
    conn.execute(format!("VACUUM INTO '{}'", staging.replace('\'', "''")), ()).await?;
    std::fs::rename(&staging, path)?;
    conn.execute("INSERT INTO snapshots(seq,path) VALUES (?,?)", turso::params![seq, path]).await?;
    Ok(())
}

/// Runtime notifications share an event identity across parent, link, and monitor delivery.
pub(crate) async fn notify(
    conn: &Connection,
    id: &str,
    seq: i64,
    reason: &str,
    error: Option<&str>,
    stopped: bool,
    initiator: Option<&str>,
) -> Result<()> {
    let counter = meta(conn, "event_counter").await?.parse::<i64>()?.checked_add(1).context("event counter overflow")?;
    set_meta(conn, "event_counter", &counter.to_string()).await?;
    let generation: i64 = meta(conn, "generation").await?.parse()?;
    let event = format!("{id}:{generation}:{counter}");
    let mut msg =
        serde_json::json!({"from":id,"child":id,"seq":seq,"reason":reason,"generation":generation,"event":event,"initiator":initiator});
    if let Some(error) = error {
        let parent = meta(conn, "parent").await?;
        if !parent.is_empty() {
            msg["type"] = "poison".into();
            msg["error"] = error.into();
            enqueue(conn, seq, &parent, &serde_json::to_vec(&msg)?).await?;
        }
    }
    if stopped {
        let watchers = query(conn, "SELECT ref,watcher FROM monitored_by ORDER BY ref", ()).await?;
        for row in watchers.rows {
            msg["type"] = "down".into();
            msg["ref"] = row.get::<String>(0)?.into();
            enqueue(conn, seq, &format!("down:{}", row.get::<String>(1)?), &serde_json::to_vec(&msg)?).await?;
        }
    }
    if stopped && reason != "normal" {
        let links = if reason == "shutdown" {
            query(conn, "SELECT peer FROM links WHERE NOT EXISTS (SELECT 1 FROM children WHERE children.id=links.peer) ORDER BY peer", ())
                .await?
        } else {
            query(conn, "SELECT peer FROM links ORDER BY peer", ()).await?
        };
        for row in links.rows {
            msg["type"] = "exit".into();
            enqueue(conn, seq, &format!("exit:{}", row.get::<String>(0)?), &serde_json::to_vec(&msg)?).await?;
        }
    }
    if stopped {
        let children =
            query(conn, "SELECT children.id FROM children JOIN links ON links.peer=children.id ORDER BY children.rowid", ()).await?;
        for row in children.rows {
            let target = if reason == "shutdown" {
                format!("shutdown:{}", row.get::<String>(0)?)
            } else {
                format!("stop:{}", row.get::<String>(0)?)
            };
            enqueue(conn, seq, &target, reason.as_bytes()).await?;
        }
    }
    Ok(())
}

pub(crate) async fn enqueue(conn: &Connection, seq: i64, target: &str, msg: &[u8]) -> Result<()> {
    let rows = query(conn, "SELECT MAX(?,COALESCE(MAX(seq),0)) FROM outbox", [seq]).await?;
    let seq: i64 = rows.rows.first().context("missing outbox sequence")?.get(0)?;
    let rows = query(conn, "SELECT COALESCE(MAX(idx),-1)+1 FROM outbox WHERE seq=?", [seq]).await?;
    let idx: i64 = rows.rows.first().context("missing notification index")?.get(0)?;
    conn.execute("INSERT INTO outbox(seq,idx,target,msg) VALUES (?,?,?,?)", turso::params![seq, idx, target, msg]).await?;
    Ok(())
}

pub(crate) async fn stop_state(conn: &Connection, id: &str, reason: &str, key: &str, initiator: &str) -> Result<()> {
    if crate::supervision::applied(conn, key).await? {
        return Ok(());
    }
    if status(conn).await? != Status::Stopped {
        set_meta(conn, "status", "stopped").await?;
        conn.execute("DELETE FROM timers", ()).await?;
        set_meta(conn, "reason", reason).await?;
        notify(conn, id, cursor(conn).await?, reason, None, true, Some(initiator)).await?;
    }
    set_meta(conn, &format!("applied:{key}"), "1").await?;
    Ok(())
}
