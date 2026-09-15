//! A durable mailbox around one host-approved native process incarnation.
use crate::{Behavior, Cap, Ctx, Trap, drivers::process::Command};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub const HASH: &str = "process-v1";
pub struct ProcessActor;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Init {
    process: String,
    #[serde(default)]
    expected_driver: Option<String>,
    #[serde(default)]
    subscriber: Option<Vec<u8>>,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Message {
    Stdin { data: String },
    CloseStdin,
    Cancel,
    Subscribe { cap: Vec<u8> },
}
fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, Trap> {
    serde_json::from_value(value).map_err(|error| Trap::new(format!("process message: {error}")))
}
fn encode(value: &impl serde::Serialize) -> Result<Vec<u8>, Trap> {
    serde_json::to_vec(value).map_err(|error| Trap::new(error.to_string()))
}
pub(crate) async fn subscribe(cx: &mut Ctx<'_>, bytes: Vec<u8>) -> Result<(), Trap> {
    let cap: Cap = serde_json::from_slice(&bytes).map_err(|error| Trap::new(format!("process subscriber capability: {error}")))?;
    cx.accept(cap.clone()).await?;
    cx.sql(
        "INSERT OR REPLACE INTO process_subscribers(cap_id,cap) VALUES (?,?)",
        turso::params![cap.cap_id.to_string(), serde_json::to_string(&cap).map_err(|e| Trap::new(e.to_string()))?],
    )
    .await?;
    Ok(())
}
async fn publish(cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
    let rows = cx.sql("SELECT cap FROM process_subscribers ORDER BY cap_id", ()).await?;
    for row in rows.rows {
        let token: String = row.get(0).map_err(|e| cx.runtime(e))?;
        let cap: Cap = serde_json::from_str(&token).map_err(|e| cx.runtime(e))?;
        cx.send(&cap, msg).await?;
    }
    Ok(())
}

impl Ctx<'_> {
    /// Record host resolution so committed spawns pin an executable instead of
    /// consulting a mutable public name when the outbox is delivered.
    pub async fn process_driver(&mut self, name: &str) -> Result<String, Trap> {
        let bytes = self.cap_operation(crate::cap_ops::Operation::ResolveProcess { name: name.into() }).await?;
        serde_json::from_slice(&bytes).map_err(|error| self.runtime(error))
    }
}

#[async_trait]
impl Behavior for ProcessActor {
    fn hash(&self) -> &str {
        HASH
    }
    fn description(&self) -> &str {
        "Durable stdin, output, and exit mailbox for a host-approved process."
    }
    fn schema(&self) -> &str {
        "CREATE TABLE IF NOT EXISTS process_state(id INTEGER PRIMARY KEY CHECK(id=1), preset TEXT NOT NULL, preset_name TEXT NOT NULL, driver_cap TEXT NOT NULL, phase TEXT NOT NULL, process_id TEXT, code INTEGER, error TEXT);
         CREATE TABLE IF NOT EXISTS process_events(seq INTEGER PRIMARY KEY, body BLOB NOT NULL, stream TEXT, bytes BLOB);
         CREATE TABLE IF NOT EXISTS process_subscribers(cap_id TEXT PRIMARY KEY, cap TEXT NOT NULL);"
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let value: Value = serde_json::from_slice(msg).map_err(|e| Trap::new(format!("process message JSON: {e}")))?;
        let rows = cx.sql("SELECT driver_cap,phase FROM process_state WHERE id=1", ()).await?;
        let Some(state) = rows.rows.first() else {
            let init: Init = parse(value)?;
            // Resolution occurs before commit; missing/unauthorized presets do
            // not open a resource or leave a live driver capability behind.
            let driver_hash = cx.process_driver(&init.process).await?;
            // Auto-provisioned actors can persist their authorized preset
            // identity in the initial inbox before their first turn executes.
            if let Some(expected) = &init.expected_driver {
                if expected != &driver_hash {
                    return Err(Trap::new(format!(
                        "process preset {} changed before initialization: expected {expected}, resolved {driver_hash}",
                        init.process
                    )));
                }
            }
            let cap = cx.spawn_driver(&driver_hash, &[]).await?;
            cx.sql(
                "INSERT INTO process_state(id,preset,preset_name,driver_cap,phase) VALUES (1,?,?,?,'starting')",
                turso::params![driver_hash, init.process, serde_json::to_string(&cap).map_err(|e| Trap::new(e.to_string()))?],
            )
            .await?;
            if let Some(subscriber) = init.subscriber {
                subscribe(cx, subscriber).await?;
            }
            return Ok(());
        };
        let token: String = state.get(0).map_err(|e| cx.runtime(e))?;
        let phase: String = state.get(1).map_err(|e| cx.runtime(e))?;
        let cap: Cap = serde_json::from_str(&token).map_err(|e| cx.runtime(e))?;
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if kind.starts_with("process.") || kind == "down" {
            if cx.sender().as_deref() != Some(cap.target.as_str()) {
                return Err(Trap::new("process event sender is not its native driver"));
            }
            match kind {
                "process.started" => {
                    let id = value["process_id"].as_str().ok_or_else(|| Trap::new("process started event missing id"))?;
                    cx.sql(
                        "UPDATE process_state SET process_id=?,phase=CASE WHEN phase='starting' THEN 'running' ELSE phase END WHERE id=1",
                        [id],
                    )
                    .await?;
                }
                "process.output" => {
                    let stream = value["stream"]
                        .as_str()
                        .filter(|stream| matches!(*stream, "stdout" | "stderr"))
                        .ok_or_else(|| Trap::new("invalid process output stream"))?;
                    let bytes: Vec<u8> = parse(value["bytes"].clone())?;
                    cx.sql(
                        "INSERT INTO process_events(seq,body,stream,bytes) VALUES (?,?,?,?)",
                        turso::params![cx.seq(), msg, stream, bytes],
                    )
                    .await?;
                    publish(cx, msg).await?;
                    return Ok(());
                }
                "process.exit" => {
                    let phase = value["phase"].as_str().ok_or_else(|| Trap::new("process exit missing phase"))?;
                    if !matches!(phase, "completed" | "cancelled" | "interrupted" | "failed") {
                        return Err(Trap::new("invalid terminal process phase"));
                    }
                    let code = value["code"].as_i64();
                    let error = value["error"].as_str();
                    cx.sql("UPDATE process_state SET phase=?,code=?,error=? WHERE id=1", turso::params![phase, code, error]).await?;
                }
                "down" => {
                    // Driver receipts prevent reopening an old process after a
                    // host restart. DOWN records interruption; resumption needs
                    // an explicit new actor, never an implicit command replay.
                    if matches!(phase.as_str(), "starting" | "running") {
                        let reason = value["reason"].as_str().unwrap_or("native process driver stopped");
                        let phase = if reason.contains("interrupted") { "interrupted" } else { "failed" };
                        cx.sql("UPDATE process_state SET phase=?,error=? WHERE id=1", turso::params![phase, reason]).await?;
                    }
                }
                _ => return Err(Trap::new("unknown process driver event")),
            }
            cx.sql("INSERT INTO process_events(seq,body) VALUES (?,?)", turso::params![cx.seq(), msg]).await?;
            publish(cx, msg).await?;
            return Ok(());
        }
        match parse::<Message>(value)? {
            Message::Subscribe { cap } => subscribe(cx, cap).await,
            Message::Cancel => {
                cx.stop(&cap, "process cancelled").await?;
                cx.sql("UPDATE process_state SET phase='cancelled' WHERE id=1 AND phase IN ('starting','running')", ()).await?;
                publish(cx, &encode(&json!({"type":"process.cancelled"}))?).await
            }
            message => {
                if !matches!(phase.as_str(), "starting" | "running") {
                    return Err(Trap::new(format!("process is {phase}; stdin is closed")));
                }
                let command = match message {
                    Message::Stdin { data } => {
                        if data.len() > 64 * 1024 {
                            return Err(Trap::new("process stdin exceeds 64 KiB"));
                        }
                        Command::Stdin { bytes: data.into_bytes() }
                    }
                    Message::CloseStdin => Command::CloseStdin,
                    _ => unreachable!(),
                };
                cx.send(&cap, &encode(&command)?).await
            }
        }
    }
}
