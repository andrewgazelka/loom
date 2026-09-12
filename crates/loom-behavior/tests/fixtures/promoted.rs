use loom::serde_json::{Value, json};

#[loom::schema]
pub fn schema() -> &'static str {
    "ALTER TABLE entries ADD COLUMN revision TEXT"
}

#[loom::def]
pub fn handle(msg: Vec<u8>) {
    let _: Value = loom::perform(
        "sql",
        json!({
            "sql":"INSERT INTO entries(body,revision) VALUES (?,?)",
            "params":[{"type":"blob","value":msg},{"type":"text","value":"v2"}]
        }),
    )
    .unwrap();
}
