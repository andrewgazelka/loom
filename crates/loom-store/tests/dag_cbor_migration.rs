use anyhow::Result;
use loom_store::Store;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
fn legacy(path: &std::path::Path) -> Result<Connection> {
    let c = Connection::open(path)?;
    let schema = include_str!("../src/schema.sql")
        .replacen(",codec INTEGER NOT NULL CHECK(codec IN (85,113))", "", 1)
        .replace("loom_json(c.bytes) AS bytes", "c.bytes");
    c.execute_batch(&schema)?;
    Ok(c)
}
fn put(c: &Connection, kind: &str, value: &Value) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    c.execute(
        "INSERT INTO cas VALUES (?,?,?,0)",
        params![hash, kind, bytes],
    )?;
    Ok(hash)
}
fn event(c: &Connection, value: &Value) -> Result<i64> {
    let hash = put(c, "event", value)?;
    c.execute(
        "INSERT INTO definition_records(event_hash,ts) VALUES (?,0)",
        params![hash],
    )?;
    Ok(c.last_insert_rowid())
}
#[test]
fn effects_fail_closed_without_schema_or_data_changes() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("legacy.sqlite");
    let c = legacy(&path)?;
    event(
        &c,
        &json!({"type":"effect_recorded","desc_hash":"unknown","scope":"global","occurrence":0,"result_hash":"unknown"}),
    )?;
    let before: Vec<u8> = c.query_row("SELECT bytes FROM cas", [], |r| r.get(0))?;
    drop(c);
    let error = match Store::open(&path) {
        Ok(_) => panic!("unsafe migration accepted"),
        Err(e) => e.to_string(),
    };
    assert!(error.contains("descriptor preimages"), "{error}");
    let c = Connection::open(path)?;
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM pragma_table_info('cas') WHERE name='codec'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        c.query_row("SELECT bytes FROM cas", [], |r| r.get::<_, Vec<u8>>(0))?,
        before
    );
    Ok(())
}
#[test]
fn cid_codec_and_raw_value_admission_are_checked() -> Result<()> {
    let store = Store::memory()?;
    let raw = store.put("blob", b"{\"x\":1}")?;
    assert!(store.get_value::<Value>(&raw).is_err());
    let wrong = loom_proto::cid_for_hash(&raw, 113).map_err(anyhow::Error::msg)?;
    assert!(store.get(&wrong).is_err());
    let value = store.put_value("result", &json!({"x":1}))?;
    let cid = store.reference(&value, 113)?;
    assert_eq!(
        store.get_value::<Value>(cid["$ref"].as_str().unwrap())?,
        Some(json!({"x":1}))
    );
    Ok(())
}

#[test]
fn legacy_tree_links_convert_to_dag_cbor() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("archive.sqlite");
    let c = legacy(&path)?;
    let raw = blake3::hash(b"raw file").to_hex().to_string();
    c.execute(
        "INSERT INTO cas VALUES (?,'blob',?,0)",
        params![raw, b"raw file".to_vec()],
    )?;
    let tree = put(
        &c,
        "tree",
        &json!({"entries":[{"name":"file","hash":raw,"directory":false,"executable":false}]}),
    )?;
    let value = json!({"tree":{"$ref":tree}});
    event(&c, &value)?;
    drop(c);
    let store = Store::open(&path)?;
    let events = store.definition_events(0, 100)?;
    let cid = events[0].event["tree"]["$ref"].as_str().unwrap();
    let tree: Value = store.get_value(cid)?.unwrap();
    assert!(tree["entries"][0].get("hash").is_none());
    let file = tree["entries"][0]["reference"]["$ref"].as_str().unwrap();
    assert_eq!(store.get(file)?, Some(b"raw file".to_vec()));
    assert_eq!(store.codec(file)?, Some(85));
    assert_eq!(store.definition_events(0, 100)?.len(), events.len());
    Ok(())
}

#[test]
fn identical_bytes_support_both_codecs_in_both_orders_and_reopen() -> Result<()> {
    for raw_first in [true, false] {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("shared.sqlite");
        let store = Store::open(&path)?;
        let hash = if raw_first {
            store.put("blob", &[0xf6])?
        } else {
            store.put_value("result", &Value::Null)?
        };
        let second = if raw_first {
            store.put_value("result", &Value::Null)?
        } else {
            store.put("blob", &[0xf6])?
        };
        assert_eq!(hash, second);
        assert_eq!(store.get_value::<Value>(&hash)?, Some(Value::Null));
        let raw = store.reference(&hash, 85)?;
        let dag = store.reference(&hash, 113)?;
        assert_ne!(raw, dag);
        drop(store);
        let store = Store::open(&path)?;
        assert_eq!(store.get(raw["$ref"].as_str().unwrap())?, Some(vec![0xf6]));
        assert_eq!(store.get(dag["$ref"].as_str().unwrap())?, Some(vec![0xf6]));
        assert!(
            store
                .get_value::<Value>(raw["$ref"].as_str().unwrap())
                .is_err()
        );
        assert_eq!(
            store.get_value::<Value>(dag["$ref"].as_str().unwrap())?,
            Some(Value::Null)
        );
        assert_eq!(store.codec(&hash)?, Some(if raw_first { 85 } else { 113 }));
        store.with_connection(|c| {
            assert_eq!(
                c.query_row("SELECT count(*) FROM cas", [], |r| r.get::<_, i64>(0))?,
                1
            );
            Ok(())
        })?;
    }
    Ok(())
}
