use loom::serde_json::{Value, json};

pub const LOOM_SCHEMA: &str =
    "CREATE TABLE IF NOT EXISTS counter (id INTEGER PRIMARY KEY, value INTEGER NOT NULL)";

pub fn handle(_msg: Vec<u8>) {
    let request = json!({
        "sql": "INSERT INTO counter(id,value) VALUES (1,2) ON CONFLICT(id) DO UPDATE SET value=value+2",
        "params": [],
    });
    let _: Value = loom::perform("sql", request).unwrap();
}
