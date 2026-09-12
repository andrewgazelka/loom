use loom::serde_json::Value;

pub const LOOM_SCHEMA: &str = "ALTER TABLE entries ADD COLUMN revision TEXT";

pub fn handle(msg: Vec<u8>) {
    let mut blob = loom::serde_json::Map::new();
    blob.insert("type".into(), Value::from("blob"));
    blob.insert("value".into(), loom::serde_json::to_value(msg).unwrap());
    let mut revision = loom::serde_json::Map::new();
    revision.insert("type".into(), Value::from("text"));
    revision.insert("value".into(), Value::from("v2"));
    let mut args = loom::serde_json::Map::new();
    args.insert(
        "sql".into(),
        Value::from("INSERT INTO entries(body,revision) VALUES (?,?)"),
    );
    args.insert(
        "params".into(),
        Value::Array([Value::Object(blob), Value::Object(revision)].into()),
    );
    let _: Value = loom::perform("sql", Value::Object(args)).unwrap();
}
