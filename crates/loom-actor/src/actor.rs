use crate::{Behavior, Ctx, EffectHandler, Registry, Rows, Status, Trap};
use anyhow::{Context, Result, anyhow, ensure};
use std::{future::Future, panic::AssertUnwindSafe, path::Path, sync::Arc, task::Poll};
use tokio::sync::Mutex;
use turso::{Connection, IntoParams};

#[derive(Clone)]
pub struct Actor {
    pub(crate) id: String,
    pub(crate) conn: Arc<Mutex<Connection>>,
}

pub(crate) async fn connect(path: &Path) -> Result<Connection> {
    let path = path.to_str().context("database path is not UTF-8")?;
    let db = turso::Builder::new_local(path).experimental_vacuum(true).build().await?;
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

pub(crate) fn behavior(registry: &Registry, hash: &str) -> Result<Arc<dyn Behavior>> {
    registry.get(hash).cloned().ok_or_else(|| anyhow!("unregistered behavior {hash}"))
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
        query(&conn, sql, params).await.with_context(|| format!("actor {} seq -1: SQL", self.id))
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
) -> Result<(), Trap> {
    let runtime = |e: anyhow::Error| Trap { message: format!("actor {id} seq {}: {e:#}", message.seq), runtime: true };
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
    let result = handle(behavior, &mut cx, &message.msg).await;
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
        return crate::mailbox::defer(conn, id, message.seq).await;
    }
    crate::mailbox::complete(&tx, message.seq).await.map_err(runtime)?;
    set_meta(&tx, &format!("code_at:{}", message.seq), &revision.to_string()).await.map_err(runtime)?;
    tx.commit().await.map_err(|e| runtime(e.into()))
}

pub(crate) async fn poison(
    conn: &mut Connection,
    id: &str,
    message: &Message,
    error: &str,
    behavior: &dyn Behavior,
    effects: &dyn EffectHandler,
) -> Result<()> {
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
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn promote(
    conn: &mut Connection,
    behavior: &dyn Behavior,
    author: &str,
    rationale: &str,
    effects: &dyn EffectHandler,
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
    tx.commit().await?;
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

pub(crate) async fn initialize(path: &Path, id: &str, parent: &str, behavior: &dyn Behavior, msg: &[u8]) -> Result<()> {
    let staging = path.with_extension(format!("creating-{}", ulid::Ulid::new()));
    let mut conn = connect(&staging).await?;
    let tx = conn.transaction().await?;
    tx.execute_batch(crate::SCHEMA).await?;
    set_meta(&tx, "id", id).await?;
    set_meta(&tx, "parent", parent).await?;
    set_meta(&tx, "node_root", if parent.is_empty() { "true" } else { "false" }).await?;
    set_meta(&tx, "ready", if parent.is_empty() { "true" } else { "false" }).await?;
    set_meta(&tx, "cursor", "0").await?;
    set_meta(&tx, "commit_epoch", "0").await?;
    set_meta(&tx, "boundary:0", "0").await?;
    set_meta(&tx, "hook_counter", "0").await?;
    set_meta(&tx, "memory_max", "0").await?;
    set_meta(&tx, "fuel", "0").await?;
    set_meta(&tx, "status", "running").await?;
    set_meta(&tx, "strategy", "park").await?;
    set_meta(&tx, "trap_exit", "false").await?;
    set_meta(&tx, "generation", "0").await?;
    set_meta(&tx, "event_counter", "0").await?;
    set_meta(&tx, "reason", "").await?;
    set_meta(&tx, "init", &serde_json::to_string(msg)?).await?;
    tx.execute_batch(behavior.schema()).await?;
    tx.execute(
        "INSERT INTO code_changes(seq,behavior_hash,author,rationale,schema_sql) VALUES (0,?,'runtime','spawn',?)",
        [behavior.hash(), behavior.schema()],
    )
    .await?;
    inject(&tx, "init", parent, msg).await?;
    tx.commit().await?;
    let ready = staging.with_extension("ready");
    let ready_str = ready.to_str().context("actor path is not UTF-8")?;
    conn.execute(format!("VACUUM INTO '{}'", ready_str.replace('\'', "''")), ()).await?;
    std::fs::rename(&ready, path)?;
    drop(conn);
    std::fs::remove_file(&staging)?;
    let wal = format!("{}-wal", staging.display());
    if Path::new(&wal).exists() {
        std::fs::remove_file(wal)?;
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
