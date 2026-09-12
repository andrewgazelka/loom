use loom::serde_json::Value;

pub const LOOM_SCHEMA: &str = "ALTER TABLE entries ADD COLUMN revision TEXT";

pub fn handle(msg: Vec<u8>) {
    let mut request: Value = loom::serde_json::from_str(
        r#"{
        "sql":"INSERT INTO entries(body,revision) VALUES (?,?)",
        "params":[{"type":"blob","value":null},{"type":"text","value":"v2"}]
    }"#,
    )
    .unwrap();
    request["params"][0]["value"] = Value::Array(msg.into_iter().map(Value::from).collect());
    let _: Value = loom::perform("sql", request).unwrap();
}
