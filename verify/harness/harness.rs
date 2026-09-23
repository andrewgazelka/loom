//! A verified Loom actor: the tool-permission core of an agent harness.
//!
//! `core` is pure and is what `lean/` proves things about (Charon starts from
//! `crate::core`, so the shell below is never translated). The shell is the Loom
//! part: it loads the state from the actor's SQLite, calls `core::step`, and
//! writes the new state plus the emitted effects back. Loom runs each message in
//! one transaction, so a crash never leaves half a step behind.
//!
//! Send JSON messages: {"tool_use":7} {"permission":[7,true]} {"tool_done":7} {"cancel":null}

pub mod core {
    pub enum Phase {
        Asked,
        Running,
        Done,
    }

    pub enum Outcome {
        Ok,
        Denied,
        Cancelled,
    }

    pub struct Call {
        pub id: u64,
        pub phase: Phase,
    }

    pub struct Harness {
        pub calls: Vec<Call>,
        pub cancelled: bool,
    }

    pub enum Event {
        /// The model asked to run tool call `id`.
        ToolUse(u64),
        /// A permission answer for `id`. The last field says whether it came from the
        /// user; answers from anyone else are ignored, and the proofs rely on that.
        Permission(u64, bool, bool),
        /// The tool process for `id` finished.
        ToolDone(u64),
        /// The user pressed cancel.
        Cancel,
    }

    pub enum Effect {
        AskPermission(u64),
        RunTool(u64),
        AbortTool(u64),
        /// The tool result sent back to the model. Exactly one per call.
        Result(u64, Outcome),
    }

    /// Storage code for a phase; `phase_of_code` inverts it (proved in `lean/`).
    pub fn phase_code(p: &Phase) -> u8 {
        match p {
            Phase::Asked => 0,
            Phase::Running => 1,
            Phase::Done => 2,
        }
    }

    /// `None` for a code no phase produces, so corrupt rows are refused, not guessed.
    pub fn phase_of_code(code: u8) -> Option<Phase> {
        match code {
            0 => Some(Phase::Asked),
            1 => Some(Phase::Running),
            2 => Some(Phase::Done),
            _ => None,
        }
    }

    /// Index of the first call with this id, or `calls.len()` when absent.
    pub fn find(calls: &Vec<Call>, id: u64) -> usize {
        let mut i = 0;
        while i < calls.len() {
            if calls[i].id == id {
                return i;
            }
            i += 1;
        }
        calls.len()
    }

    pub fn cancel_all(calls: &mut Vec<Call>, out: &mut Vec<Effect>) {
        let mut i = 0;
        while i < calls.len() {
            let id = calls[i].id;
            match calls[i].phase {
                Phase::Asked => {
                    calls[i].phase = Phase::Done;
                    out.push(Effect::Result(id, Outcome::Cancelled));
                }
                Phase::Running => {
                    calls[i].phase = Phase::Done;
                    out.push(Effect::AbortTool(id));
                    out.push(Effect::Result(id, Outcome::Cancelled));
                }
                Phase::Done => {}
            }
            i += 1;
        }
    }

    pub fn step(h: &mut Harness, e: Event) -> Vec<Effect> {
        let mut out = Vec::new();
        match e {
            Event::ToolUse(id) => {
                if find(&h.calls, id) == h.calls.len() {
                    if h.cancelled {
                        h.calls.push(Call { id, phase: Phase::Done });
                        out.push(Effect::Result(id, Outcome::Cancelled));
                    } else {
                        h.calls.push(Call { id, phase: Phase::Asked });
                        out.push(Effect::AskPermission(id));
                    }
                }
            }
            Event::Permission(id, allow, from_user) => {
                let i = find(&h.calls, id);
                if from_user && i < h.calls.len() {
                    if let Phase::Asked = h.calls[i].phase {
                        if allow {
                            h.calls[i].phase = Phase::Running;
                            out.push(Effect::RunTool(id));
                        } else {
                            h.calls[i].phase = Phase::Done;
                            out.push(Effect::Result(id, Outcome::Denied));
                        }
                    }
                }
            }
            Event::ToolDone(id) => {
                let i = find(&h.calls, id);
                if i < h.calls.len() {
                    if let Phase::Running = h.calls[i].phase {
                        h.calls[i].phase = Phase::Done;
                        out.push(Effect::Result(id, Outcome::Ok));
                    }
                }
            }
            Event::Cancel => {
                h.cancelled = true;
                cancel_all(&mut h.calls, &mut out);
            }
        }
        out
    }

    /// The first draft, kept to show what the checker finds. Cancel reports every
    /// open call as cancelled but forgets to move it to `Done`, so a late
    /// permission still runs the tool and a late `ToolDone` sends a second result.
    pub fn step_v1(h: &mut Harness, e: Event) -> Vec<Effect> {
        let mut out = Vec::new();
        match e {
            Event::Cancel => {
                h.cancelled = true;
                let mut i = 0;
                while i < h.calls.len() {
                    let id = h.calls[i].id;
                    match h.calls[i].phase {
                        Phase::Asked => out.push(Effect::Result(id, Outcome::Cancelled)),
                        Phase::Running => {
                            out.push(Effect::AbortTool(id));
                            out.push(Effect::Result(id, Outcome::Cancelled));
                        }
                        Phase::Done => {}
                    }
                    i += 1;
                }
            }
            other => out = step(h, other),
        }
        out
    }
}

// ---- Loom shell: IO lives here, nothing below is proved ----
//
// Trusted, and narrower than the proof: ids must fit in i64 (SQLite INTEGER). The user
// is whoever Loom reports as sender `loom::actor::EXTERNAL`: a message sent through the
// node API or CLI with the node token. Messages from other actors (the model, tool workers) arrive
// with their actor id and cannot approve anything; `core::step` enforces that.

use core::{Call, Effect, Event, Harness, Outcome};
use loom::serde_json::{Value, json};

pub const LOOM_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS harness_calls (pos INTEGER PRIMARY KEY, id INTEGER NOT NULL, phase INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS harness_flags (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS harness_effects (seq INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, id INTEGER NOT NULL, outcome TEXT);
CREATE TABLE IF NOT EXISTS harness_rejected (seq INTEGER PRIMARY KEY AUTOINCREMENT, msg BLOB NOT NULL, reason TEXT NOT NULL)";

/// SQL parameters and result cells are tagged: {"type":"integer","value":7}.
fn int(v: i64) -> Value {
    json!({"type": "integer", "value": v})
}

fn text(v: &str) -> Value {
    json!({"type": "text", "value": v})
}

fn null() -> Value {
    json!({"type": "null"})
}

fn cell_int(cell: &Value) -> i64 {
    assert_eq!(cell["type"], "integer", "expected an integer cell, got {cell}");
    cell["value"].as_i64().unwrap()
}

/// Ids are u64 in `core` and i64 in SQLite; the cast is a bijection, so equality survives.
fn to_sql(id: u64) -> Value {
    int(id as i64)
}

fn sql(query: &str, params: Vec<Value>) -> Value {
    loom::perform("sql", json!({"sql": query, "params": params})).unwrap()
}

fn load() -> Harness {
    let rows = sql("SELECT id, phase FROM harness_calls ORDER BY pos", vec![]);
    let mut calls = Vec::new();
    for row in rows["rows"].as_array().unwrap() {
        let code = cell_int(&row[1]);
        let phase = u8::try_from(code).ok().and_then(core::phase_of_code).unwrap_or_else(|| panic!("unknown stored phase {code}"));
        calls.push(Call { id: cell_int(&row[0]) as u64, phase });
    }
    let flag = sql("SELECT 1 FROM harness_flags WHERE name = 'cancelled' AND value = 1", vec![]);
    let cancelled = !flag["rows"].as_array().unwrap().is_empty();
    Harness { calls, cancelled }
}

fn store(h: &Harness, effects: &[Effect]) {
    sql("DELETE FROM harness_calls", vec![]);
    for (pos, call) in h.calls.iter().enumerate() {
        let phase = i64::from(core::phase_code(&call.phase));
        sql("INSERT INTO harness_calls (pos, id, phase) VALUES (?, ?, ?)", vec![int(pos as i64), to_sql(call.id), int(phase)]);
    }
    if h.cancelled {
        sql("INSERT OR IGNORE INTO harness_flags (name, value) VALUES ('cancelled', 1)", vec![]);
    }
    for effect in effects {
        let (kind, id, outcome) = match effect {
            Effect::AskPermission(id) => ("ask_permission", *id, null()),
            Effect::RunTool(id) => ("run_tool", *id, null()),
            Effect::AbortTool(id) => ("abort_tool", *id, null()),
            Effect::Result(id, outcome) => {
                let outcome = match outcome {
                    Outcome::Ok => "ok",
                    Outcome::Denied => "denied",
                    Outcome::Cancelled => "cancelled",
                };
                ("result", *id, text(outcome))
            }
        };
        sql("INSERT INTO harness_effects (kind, id, outcome) VALUES (?, ?, ?)", vec![text(kind), to_sql(id), outcome]);
    }
}

/// Exactly one of the four message shapes, or the reason it is not one. A malformed
/// message is recorded in `harness_rejected` and changes nothing: trapping would park
/// the whole actor, and treating it as a cancel would end the session.
fn parse(msg: &[u8], from_user: bool) -> Result<Event, String> {
    let value: Value = loom::serde_json::from_slice(msg).map_err(|e| format!("not JSON: {e}"))?;
    let object = value.as_object().ok_or("not a JSON object")?;
    let [(key, arg)] = object.iter().collect::<Vec<_>>()[..] else {
        return Err(format!("expected exactly one key, got {}", object.len()));
    };
    let id = |v: &Value| v.as_u64().filter(|id| i64::try_from(*id).is_ok()).ok_or("id must be an integer below 2^63");
    match key.as_str() {
        "tool_use" => Ok(Event::ToolUse(id(arg)?)),
        "permission" => {
            let allow = arg[1].as_bool().ok_or("permission takes [id, bool]")?;
            Ok(Event::Permission(id(&arg[0])?, allow, from_user))
        }
        "tool_done" => Ok(Event::ToolDone(id(arg)?)),
        "cancel" => Ok(Event::Cancel),
        other => Err(format!("unknown message {other:?}")),
    }
}

pub fn handle(msg: Vec<u8>) {
    let from_user = loom::actor::sender().unwrap().as_deref() == Some(loom::actor::EXTERNAL);
    let event = match parse(&msg, from_user) {
        Ok(event) => event,
        Err(reason) => {
            sql("INSERT INTO harness_rejected (msg, reason) VALUES (?, ?)", vec![json!({"type": "blob", "value": msg}), text(&reason)]);
            return;
        }
    };
    let mut h = load();
    let effects = core::step(&mut h, event);
    store(&h, &effects);
}
