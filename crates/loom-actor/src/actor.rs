mod inspection;
pub(crate) use inspection::{inspect_query, inspect_statement};
mod initialize;
mod snapshot;
pub(crate) use snapshot::{compact_cdc, replace_snapshot, snapshot};
use crate::{Behavior, Ctx, EffectHandler, Registry, Rows, Status, Trap};
use anyhow::{Context, Result, anyhow, ensure};
pub(crate) use initialize::initialize;
use std::{future::Future, panic::AssertUnwindSafe, path::Path, sync::Arc, task::Poll};
use tokio::sync::Mutex;
use turso::{Connection, IntoParams};

#[derive(Clone)]
pub struct Actor {
    pub(crate) id: String,
    pub(crate) conn: Arc<Mutex<Connection>>,
    pub(crate) managed: bool,
}

pub(crate) async fn connect(path: &Path, io: crate::Io) -> Result<Connection> {
    let path = path.to_str().context("database path is not UTF-8")?;
    let db = turso::Builder::new_local(path).with_io(io.name()?.to_owned()).experimental_vacuum(true).build().await?;
    let conn = db.connect()?;
    query(&conn, "PRAGMA journal_mode=WAL", ()).await?;
    query(&conn, "PRAGMA synchronous=NORMAL", ()).await?;
    query(&conn, "PRAGMA capture_data_changes_conn = 'full'", ()).await?;
    let cdc = query(&conn, "PRAGMA table_info(turso_cdc)", ()).await?;
    ensure!(
        cdc.rows.iter().any(|row| row.get::<String>(1).is_ok_and(|name| name == "change_txn_id")),
        "actor {path} seq -1: capture_data_changes_conn requires CDC v2 change_txn_id"
    );
    let runtime = query(&conn, "SELECT name FROM sqlite_schema WHERE type='table' AND name='meta'", ()).await?;
    if !runtime.rows.is_empty() {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS subscribers(id TEXT PRIMARY KEY,subscriber TEXT NOT NULL,\"table\" TEXT NOT NULL,\
             after_change_id INTEGER NOT NULL)",
            (),
        )
        .await?;
        conn.execute("INSERT OR IGNORE INTO meta(key,value) VALUES ('cdc_floor','0')", ()).await?;
    }
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
        query(&conn, sql, params).await.with_context(|| format!("actor {} seq -1: SQL", self.id))
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

/// Poll-by-poll panic boundary includes the async-trait method's initial call.
async fn handle(behavior: &dyn Behavior, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
    let mut future = Box::pin(async { behavior.handle(cx, msg).await });
    std::future::poll_fn(|task| match std::panic::catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(task))) {
        Ok(result) => result,
        Err(payload) => {
            let message = if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_owned()
            } else {
                "handler panicked with non-string payload".into()
            };
            Poll::Ready(Err(Trap::new(message)))
        }
    })
    .await
}

pub(crate) async fn attempt(
    conn: &mut Connection,
    id: &str,
    message: &Message,
    behavior: &dyn Behavior,
    revision: i64,
    effects: &dyn EffectHandler,
    control: Option<&crate::durability::AttemptControl<'_>>,
) -> Result<bool, Trap> {
    let runtime = |e: anyhow::Error| Trap { message: format!("actor {id} seq {}: {e:#}", message.seq), runtime: true, durability: false };
    let schema = crate::Node::schema_fingerprint(conn).await.map_err(runtime)?;
    let tx = conn.transaction().await.map_err(|e| runtime(e.into()))?;
    let mut cx = Ctx {
        conn: &tx,
        actor_id: id,
        seq: message.seq,
        idx: 0,
        random_counter: 0,
        effects,
        failure: None,
        deferred: false,
        sender: message.sender.clone(),
        generation: meta(&tx, "generation").await.map_err(runtime)?.parse().map_err(|e| runtime(anyhow!("invalid generation: {e}")))?,
    };
    // Cancellation is confined to the handler. Transaction completion must be
    // driven to completion even when a kill arrives during asynchronous I/O.
    let result = tokio::select! {
        result = handle(behavior, &mut cx, &message.msg) => Some(result),
        _ = async { match control.map(|c| c.cancellation) { Some(signal) => signal.notified().await, None => std::future::pending().await } } => None,
    };
    let Some(result) = result else {
        drop(cx);
        tx.rollback().await.map_err(|error| runtime(error.into()))?;
        return Ok(false);
    };
    let result = Trap::finish(cx.failure.take(), result);
    let deferred = cx.deferred;
    drop(cx);
    if let Err(mut error) = result {
        tx.rollback().await.map_err(|e| runtime(e.into()))?;
        error.message = format!("actor {id} seq {}: {}", message.seq, error.message);
        return Err(error);
    }
    if deferred {
        tx.rollback().await.map_err(|e| runtime(e.into()))?;
        return crate::mailbox::defer(conn, id, message.seq, control.map(|c| c.node)).await.map(|()| true);
    }
    crate::mailbox::complete(&tx, message.seq).await.map_err(runtime)?;
    set_meta(&tx, &format!("code_at:{}", message.seq), &revision.to_string()).await.map_err(runtime)?;
    mark_schema_snapshot(&tx, &schema).await.map_err(runtime)?;
    crate::subscribe::record_origin(&tx, id, message.seq).await.map_err(runtime)?;
    if let Some(control) = control
        && let Err(error) = control.node.prepare_commit(id, &tx).await
    {
        tx.rollback().await.map_err(|e| runtime(e.into()))?;
        return Err(Trap { message: format!("actor {id}: {error:#}"), runtime: true, durability: true });
    }
    tx.commit().await.map_err(|e| {
        let mut error = runtime(e.into());
        error.durability = control.is_some_and(|c| c.node.remote.is_some());
        error
    })?;
    Ok(true)
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
    let schema = crate::Node::schema_fingerprint(conn).await?;
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
    mark_schema_snapshot(&tx, &schema).await?;
    if let Some(node) = node {
        node.commit_control(&meta(&tx, "id").await?, tx).await?;
    } else {
        tx.commit().await?;
    }
    Ok(())
}

async fn mark_schema_snapshot(conn: &Connection, before: &str) -> Result<()> {
    if crate::Node::schema_fingerprint(conn).await? != before {
        // Node::snapshot_schema_change clears this only after publishing a matching schema snapshot.
        set_meta(conn, "schema_snapshot_pending", "1").await?;
    }
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
