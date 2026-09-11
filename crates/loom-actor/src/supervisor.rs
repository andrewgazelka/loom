//! A durable supervisor implemented entirely as a normal actor behavior.
use crate::{Behavior, ChildSpec, ChildState, ChildType, Ctx, RestartPolicy, RestartVerb, Trap, Value};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Default supervision policy; configuration and restart history live in its file.
#[derive(Clone, Copy, Debug, Default)]
pub struct Supervisor;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS spec(child_id TEXT PRIMARY KEY, "order" INTEGER, behavior_hash TEXT, init BLOB, restart TEXT, shutdown TEXT, link INTEGER, type TEXT, monitor INTEGER);
INSERT OR REPLACE INTO meta(key,value) VALUES ('trap_exit','true');
UPDATE meta SET value='one_for_one' WHERE key='strategy' AND value IN ('park','skip','stop');
INSERT OR IGNORE INTO meta(key,value) VALUES ('strategy','one_for_one'),('max_restarts','3'),('max_seconds','5');
"#;

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Strategy {
    OneForOne,
    OneForAll,
    RestForOne,
    Dynamic,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Command {
    Configure { strategy: Option<Strategy>, max_restarts: Option<i64>, max_seconds: Option<i64>, template: Option<ChildSpec> },
    StartChild { spec: Option<ChildSpec>, init: Option<Vec<u8>> },
    TerminateChild { id: String },
    RestartChild { id: String },
    DeleteChild { id: String },
    CountChildren,
    WhichChildren,
    Down { from: String, reason: String, generation: i64, event: String, initiator: Option<String> },
    Exit { from: String, reason: String, generation: i64, event: String, initiator: Option<String> },
    Poison { child: String, generation: i64, event: String },
}

struct SpecRow {
    id: String,
    order: i64,
    spec: ChildSpec,
}
struct Failure {
    child: String,
    reason: String,
    generation: i64,
    event: String,
    initiator: Option<String>,
}
struct Restart {
    id: String,
    verb: RestartVerb,
}
#[derive(Serialize)]
struct ChildDescription {
    id: String,
    order: i64,
    #[serde(flatten)]
    spec: ChildSpec,
    status: crate::Status,
    generation: i64,
}

fn error(cx: &Ctx<'_>, message: impl std::fmt::Display) -> Trap {
    Trap::new(format!("actor {} seq {}: {message}", cx.self_id(), cx.seq()))
}
fn cell(cx: &Ctx<'_>, row: &turso::Row, index: usize) -> Result<Value, Trap> {
    row.get_value(index).map_err(|e| error(cx, e))
}
fn text(cx: &Ctx<'_>, row: &turso::Row, index: usize) -> Result<String, Trap> {
    match cell(cx, row, index)? {
        Value::Text(value) => Ok(value),
        _ => Err(error(cx, format!("expected text at column {index}"))),
    }
}
fn integer(cx: &Ctx<'_>, row: &turso::Row, index: usize) -> Result<i64, Trap> {
    match cell(cx, row, index)? {
        Value::Integer(value) => Ok(value),
        _ => Err(error(cx, format!("expected integer at column {index}"))),
    }
}
async fn specs(cx: &mut Ctx<'_>) -> Result<Vec<SpecRow>, Trap> {
    let rows =
        cx.sql("SELECT child_id,\"order\",behavior_hash,init,restart,shutdown,link,type,monitor FROM spec ORDER BY \"order\"", ()).await?;
    let mut result = Vec::new();
    for row in rows.rows {
        let init = match cell(cx, &row, 3)? {
            Value::Blob(value) => value,
            _ => return Err(error(cx, "child init must be a blob")),
        };
        let restart = match text(cx, &row, 4)?.as_str() {
            "permanent" => RestartPolicy::Permanent,
            "transient" => RestartPolicy::Transient,
            "temporary" => RestartPolicy::Temporary,
            _ => return Err(error(cx, "invalid child restart policy")),
        };
        let shutdown = serde_json::from_str(&text(cx, &row, 5)?).map_err(|e| error(cx, e))?;
        let link = match integer(cx, &row, 6)? {
            0 => false,
            1 => true,
            _ => return Err(error(cx, "invalid child link flag")),
        };
        result.push(SpecRow {
            id: text(cx, &row, 0)?,
            order: integer(cx, &row, 1)?,
            spec: ChildSpec {
                behavior_hash: text(cx, &row, 2)?,
                init,
                restart,
                shutdown,
                link,
                child_type: match text(cx, &row, 7)?.as_str() {
                    "worker" => ChildType::Worker,
                    "supervisor" => ChildType::Supervisor,
                    _ => return Err(error(cx, "invalid child type")),
                },
                monitor: match integer(cx, &row, 8)? {
                    0 => false,
                    1 => true,
                    _ => return Err(error(cx, "invalid child monitor flag")),
                },
            },
        });
    }
    Ok(result)
}
async fn meta(cx: &mut Ctx<'_>, key: &str) -> Result<String, Trap> {
    let rows = cx.sql("SELECT value FROM meta WHERE key=?", [key]).await?;
    let row = rows.rows.first().ok_or_else(|| error(cx, format!("missing supervisor meta {key}")))?;
    text(cx, row, 0)
}
async fn set_meta(cx: &mut Ctx<'_>, key: &str, value: &str) -> Result<(), Trap> {
    cx.sql("INSERT OR REPLACE INTO meta(key,value) VALUES (?,?)", [key, value]).await?;
    Ok(())
}
async fn configure(
    cx: &mut Ctx<'_>,
    strategy: Option<Strategy>,
    max_restarts: Option<i64>,
    max_seconds: Option<i64>,
    template: Option<ChildSpec>,
) -> Result<(), Trap> {
    if let Some(value) = strategy {
        let name = match value {
            Strategy::OneForOne => "one_for_one",
            Strategy::OneForAll => "one_for_all",
            Strategy::RestForOne => "rest_for_one",
            Strategy::Dynamic => "dynamic",
        };
        set_meta(cx, "strategy", name).await?;
    }
    if let Some(template) = template {
        let encoded = serde_json::to_string(&template).map_err(|e| error(cx, e))?;
        set_meta(cx, "template", &encoded).await?;
    }
    if meta(cx, "strategy").await? == "dynamic" {
        let _: ChildSpec = serde_json::from_str(&meta(cx, "template").await?).map_err(|e| error(cx, e))?;
    }
    if let Some(value) = max_restarts {
        if value < 0 {
            return Err(error(cx, "max_restarts must be nonnegative"));
        }
        set_meta(cx, "max_restarts", &value.to_string()).await?;
    }
    if let Some(value) = max_seconds {
        if value <= 0 || value.checked_mul(1000).is_none() {
            return Err(error(cx, "max_seconds must be positive and fit milliseconds"));
        }
        set_meta(cx, "max_seconds", &value.to_string()).await?;
    }
    Ok(())
}
/// Register an already-created child in the caller's transaction (node-root attach).
pub(crate) use crate::supervisor_store::record_child;
async fn start_child(cx: &mut Ctx<'_>, spec: Option<ChildSpec>, init: Option<Vec<u8>>) -> Result<(), Trap> {
    if spec.is_some() == init.is_some() {
        return Err(error(cx, "start_child requires exactly one of spec or init"));
    }
    let mut spec = match spec {
        Some(spec) => spec,
        None => {
            let init = init.ok_or_else(|| error(cx, "missing dynamic child init"))?;
            if meta(cx, "strategy").await? != "dynamic" {
                return Err(error(cx, "start_child(init) requires dynamic strategy"));
            }
            let mut template: ChildSpec = serde_json::from_str(&meta(cx, "template").await?).map_err(|e| error(cx, e))?;
            template.init = init;
            template
        }
    };
    spec.monitor = true;
    let id = cx.spawn(&spec).await?;
    record_child(cx.conn, &id, &spec).await.map_err(|e| cx.runtime(e))?;
    Ok(())
}
async fn manage_child(cx: &mut Ctx<'_>, id: &str, command: &str) -> Result<(), Trap> {
    if !specs(cx).await?.iter().any(|child| child.id == id) {
        return Err(error(cx, format!("unknown child {id}")));
    }
    let marker = format!("terminated:{id}");
    match command {
        "terminate" => {
            set_meta(cx, &marker, "1").await?;
            cx.shutdown(id).await?;
        }
        "restart" => {
            if cx.inspect(id).await?.status != crate::Status::Stopped {
                return Err(error(cx, "restart_child requires stopped child"));
            }
            cx.sql("DELETE FROM meta WHERE key=?", [marker]).await?;
            cx.restart(id, RestartVerb::Reset).await?;
            cx.monitor(id).await?;
        }
        "delete" => {
            if cx.inspect(id).await?.status != crate::Status::Stopped {
                return Err(error(cx, "delete_child requires stopped child"));
            }
            cx.sql("DELETE FROM spec WHERE child_id=?", [id]).await?;
            cx.sql("DELETE FROM children WHERE id=?", [id]).await?;
            cx.unlink(id).await?;
            // Keep the marker so queued lifecycle notifications remain harmless.
            set_meta(cx, &marker, "1").await?;
        }
        _ => return Err(error(cx, "invalid child management command")),
    }
    Ok(())
}
async fn count_children(cx: &mut Ctx<'_>) -> Result<(), Trap> {
    let sender = cx.sender().ok_or_else(|| error(cx, "count_children requires a sender"))?;
    let children = specs(cx).await?;
    let mut active = 0usize;
    let mut workers = 0usize;
    let mut supervisors = 0usize;
    for child in &children {
        let state = cx.inspect(&child.id).await?;
        if matches!(state.status, crate::Status::Running | crate::Status::Parked) {
            active += 1;
        }
        match &child.spec.child_type {
            ChildType::Worker => workers += 1,
            ChildType::Supervisor => supervisors += 1,
        }
    }
    let reply = serde_json::to_vec(&serde_json::json!({"type":"count_children","count":active,"active":active,
        "specs":children.len(),"workers":workers,"supervisors":supervisors}))
    .map_err(|e| error(cx, e))?;
    cx.send(&sender, &reply).await
}
async fn which_children(cx: &mut Ctx<'_>) -> Result<(), Trap> {
    let sender = cx.sender().ok_or_else(|| error(cx, "which_children requires a sender"))?;
    let mut children = Vec::new();
    for row in specs(cx).await? {
        let state = cx.inspect(&row.id).await?;
        children.push(ChildDescription {
            id: row.id,
            order: row.order,
            spec: row.spec,
            status: state.status,
            generation: state.generation,
        });
    }
    let reply = serde_json::to_vec(&serde_json::json!({ "type": "children", "children": children })).map_err(|e| error(cx, e))?;
    cx.send(&sender, &reply).await
}
fn restart_verb(state: &ChildState, policy: RestartPolicy, reason: &str) -> RestartVerb {
    if reason != "poison" {
        return RestartVerb::Reset;
    }
    if state.poison_revision.is_some_and(|revision| state.revision > revision) {
        return RestartVerb::Resume;
    }
    if matches!(policy, RestartPolicy::Permanent) { RestartVerb::Reset } else { RestartVerb::Skip }
}
async fn failure(cx: &mut Ctx<'_>, failure: Failure) -> Result<(), Trap> {
    if matches!(failure.reason.as_str(), "shutdown" | "killed") && failure.initiator.as_deref() == Some(cx.self_id()) {
        return Ok(());
    }
    if !cx.sql("SELECT value FROM meta WHERE key=?", [format!("terminated:{}", failure.child)]).await?.rows.is_empty() {
        return Ok(());
    }
    let children = specs(cx).await?;
    let failed = children
        .iter()
        .find(|row| row.id == failure.child)
        .ok_or_else(|| error(cx, format!("notification from non-child {}", failure.child)))?;
    let state = cx.inspect(&failure.child).await?;
    if state.generation != failure.generation {
        return Ok(());
    }
    let event_key = format!("supervisor_event:{}", failure.event);
    if !cx.sql("SELECT value FROM meta WHERE key=?", [event_key.as_str()]).await?.rows.is_empty() {
        return Ok(());
    }
    set_meta(cx, &event_key, "handled").await?;
    let eligible = match failed.spec.restart {
        RestartPolicy::Permanent => true,
        RestartPolicy::Transient => failure.reason != "normal",
        RestartPolicy::Temporary => false,
    };
    if !eligible {
        return Ok(());
    }
    if restart_verb(&state, failed.spec.restart, &failure.reason) == RestartVerb::Resume {
        return cx.restart(&failed.id, RestartVerb::Resume).await;
    }
    let now = cx.now().await?;
    let max_restarts: i64 = meta(cx, "max_restarts").await?.parse().map_err(|e| error(cx, e))?;
    let max_seconds: i64 = meta(cx, "max_seconds").await?.parse().map_err(|e| error(cx, e))?;
    if max_restarts < 0 || max_seconds <= 0 {
        return Err(error(cx, "invalid restart intensity"));
    }
    let window = max_seconds.checked_mul(1000).ok_or_else(|| error(cx, "restart window overflow"))?;
    let since = now.checked_sub(window).ok_or_else(|| error(cx, "restart window underflow"))?;
    let rows = cx.sql("SELECT COUNT(*) FROM restarts WHERE at>=? AND at<=?", turso::params![since, now]).await?;
    let row = rows.rows.first().ok_or_else(|| error(cx, "missing restart count"))?;
    if integer(cx, row, 0)? >= max_restarts {
        return cx.exit("shutdown").await;
    }
    cx.sql("INSERT INTO restarts(child,at) VALUES (?,?)", turso::params![failure.child.as_str(), now]).await?;
    let strategy = match meta(cx, "strategy").await?.as_str() {
        "one_for_one" => Strategy::OneForOne,
        "one_for_all" => Strategy::OneForAll,
        "rest_for_one" => Strategy::RestForOne,
        "dynamic" => Strategy::Dynamic,
        _ => return Err(error(cx, "invalid supervisor strategy")),
    };
    let mut restarts = Vec::new();
    for child in &children {
        let selected = match strategy {
            Strategy::OneForOne | Strategy::Dynamic => child.id == failed.id,
            Strategy::OneForAll => true,
            Strategy::RestForOne => child.order >= failed.order,
        };
        if !selected {
            continue;
        }
        if matches!(child.spec.restart, RestartPolicy::Temporary) {
            cx.shutdown(&child.id).await?;
            continue;
        }
        let verb = if child.id == failed.id { restart_verb(&state, child.spec.restart, &failure.reason) } else { RestartVerb::Reset };
        // Resume/Skip preserve live relationships; only Reset stops the child.
        if verb == RestartVerb::Reset {
            cx.shutdown(&child.id).await?;
        }
        restarts.push(Restart { id: child.id.clone(), verb });
    }
    for restart in restarts {
        cx.restart(&restart.id, restart.verb).await?;
        if restart.verb == RestartVerb::Reset {
            cx.monitor(&restart.id).await?;
        }
    }
    Ok(())
}

#[async_trait]
impl Behavior for Supervisor {
    fn hash(&self) -> &str {
        "supervisor-v1"
    }
    fn schema(&self) -> &str {
        SCHEMA
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let command: Command = serde_json::from_slice(msg).map_err(|e| error(cx, e))?;
        match command {
            Command::Configure { strategy, max_restarts, max_seconds, template } => {
                configure(cx, strategy, max_restarts, max_seconds, template).await
            }
            Command::StartChild { spec, init } => start_child(cx, spec, init).await,
            Command::TerminateChild { id } => manage_child(cx, &id, "terminate").await,
            Command::RestartChild { id } => manage_child(cx, &id, "restart").await,
            Command::DeleteChild { id } => manage_child(cx, &id, "delete").await,
            Command::CountChildren => count_children(cx).await,
            Command::WhichChildren => which_children(cx).await,
            Command::Exit { from, reason, generation, event, initiator } => {
                if reason == "shutdown"
                    && cx.sender().as_deref() == Some(from.as_str())
                    && !specs(cx).await?.iter().any(|child| child.id == from)
                {
                    return cx.exit("shutdown").await;
                }
                failure(cx, Failure { child: from, reason, generation, event, initiator }).await
            }
            Command::Down { from, reason, generation, event, initiator } => {
                failure(cx, Failure { child: from, reason, generation, event, initiator }).await
            }
            Command::Poison { child, generation, event } => {
                failure(cx, Failure { child, reason: "poison".into(), generation, event, initiator: None }).await
            }
        }
    }
}
