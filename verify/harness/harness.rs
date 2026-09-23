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
        /// The user answered the permission prompt for `id`.
        Permission(u64, bool),
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
            Event::Permission(id, allow) => {
                let i = find(&h.calls, id);
                if i < h.calls.len() {
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
// Trusted, and narrower than the proof: ids must fit in i64 (SQLite INTEGER), and P1
// assumes `permission` messages come only from the user. The guest API does not expose
// the sender, so the shell cannot check that yet.

use core::{Call, Effect, Event, Harness, Outcome, Phase};
use loom::serde_json::{Value, json};

pub const LOOM_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS calls (pos INTEGER PRIMARY KEY, id INTEGER NOT NULL, phase INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS flags (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS effects (seq INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, id INTEGER NOT NULL, outcome TEXT)";

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
    let rows = sql("SELECT id, phase FROM calls ORDER BY pos", vec![]);
    let mut calls = Vec::new();
    for row in rows["rows"].as_array().unwrap() {
        let phase = match cell_int(&row[1]) {
            0 => Phase::Asked,
            1 => Phase::Running,
            2 => Phase::Done,
            other => panic!("unknown stored phase {other}"),
        };
        calls.push(Call { id: cell_int(&row[0]) as u64, phase });
    }
    let flag = sql("SELECT 1 FROM flags WHERE name = 'cancelled' AND value = 1", vec![]);
    let cancelled = !flag["rows"].as_array().unwrap().is_empty();
    Harness { calls, cancelled }
}

fn store(h: &Harness, effects: &[Effect]) {
    sql("DELETE FROM calls", vec![]);
    for (pos, call) in h.calls.iter().enumerate() {
        let phase = match call.phase {
            Phase::Asked => 0,
            Phase::Running => 1,
            Phase::Done => 2,
        };
        sql("INSERT INTO calls (pos, id, phase) VALUES (?, ?, ?)", vec![int(pos as i64), to_sql(call.id), int(phase)]);
    }
    if h.cancelled {
        sql("INSERT OR IGNORE INTO flags (name, value) VALUES ('cancelled', 1)", vec![]);
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
        sql("INSERT INTO effects (kind, id, outcome) VALUES (?, ?, ?)", vec![text(kind), to_sql(id), outcome]);
    }
}

/// Exactly one of the four message shapes; anything else traps, so Loom dead-letters it
/// instead of treating a typo as a cancel.
fn parse(msg: &[u8]) -> Event {
    let value: Value = loom::serde_json::from_slice(msg).unwrap();
    let object = value.as_object().expect("message must be a JSON object");
    assert_eq!(object.len(), 1, "message must have exactly one key: {value}");
    let (key, arg) = object.iter().next().unwrap();
    match key.as_str() {
        "tool_use" => Event::ToolUse(arg.as_u64().unwrap()),
        "permission" => Event::Permission(arg[0].as_u64().unwrap(), arg[1].as_bool().unwrap()),
        "tool_done" => Event::ToolDone(arg.as_u64().unwrap()),
        "cancel" => Event::Cancel,
        other => panic!("unknown message {other:?}"),
    }
}

pub fn handle(msg: Vec<u8>) {
    let mut h = load();
    let effects = core::step(&mut h, parse(&msg));
    store(&h, &effects);
}
