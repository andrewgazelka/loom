use loom::serde_json::Value;

pub const LOOM_SCHEMA: &str =
    "CREATE TABLE IF NOT EXISTS counter (id INTEGER PRIMARY KEY, value INTEGER NOT NULL)";

pub fn handle(_msg: Vec<u8>) {
    let request: Value = loom::serde_json::from_str(
        r#"{"sql":"INSERT INTO counter(id,value) VALUES (1,1) ON CONFLICT(id) DO UPDATE SET value=value+1","params":[]}"#,
    )
    .unwrap();
    let _: Value = loom::perform("sql", request).unwrap();
}
