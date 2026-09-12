//! Transactional conversion of the legacy JSON CAS. No legacy codec remains live.
use anyhow::{Context, Result, ensure};
use loom_proto::Value;
use rusqlite::{Connection, params};
use std::collections::{BTreeMap, BTreeSet};
struct Target {
    table: &'static str,
    column: &'static str,
}
struct Object {
    kind: String,
    bytes: Vec<u8>,
    created: i64,
}
struct Converted {
    hash: String,
    codec: u64,
    bytes: Vec<u8>,
}

pub(super) fn run(c: &mut Connection) -> Result<()> {
    let exists: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='cas')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let codec: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('cas') WHERE name='codec')",
        [],
        |r| r.get(0),
    )?;
    if codec {
        let tx = c.transaction()?;
        register_codecs(&tx)?;
        tx.commit()?;
        return Ok(());
    }
    let tx = c.transaction()?;
    tx.execute_batch("PRAGMA defer_foreign_keys=ON;")?;
    let mut objects = BTreeMap::new();
    {
        let mut q = tx.prepare("SELECT hash,kind,bytes,created_at FROM cas")?;
        let mut rows = q.query([])?;
        while let Some(r) = rows.next()? {
            objects.insert(
                r.get::<_, String>(0)?,
                Object {
                    kind: r.get(1)?,
                    bytes: r.get(2)?,
                    created: r.get(3)?,
                },
            );
        }
    }
    for object in objects.values().filter(|object| object.kind == "event") {
        let event: Value = serde_json::from_slice(&object.bytes)?;
        ensure!(
            event["type"] != "effect_recorded",
            "DAG-CBOR migration refused: legacy effect descriptor preimages were not persisted"
        );
    }
    let effects: i64 = tx.query_row("SELECT count(*) FROM effect_results", [], |r| r.get(0))?;
    ensure!(
        effects == 0,
        "DAG-CBOR migration refused: legacy effect descriptor preimages unavailable; retain original database and reconcile effects"
    );
    let mut converted = BTreeMap::new();
    for hash in objects.keys() {
        convert(hash, &objects, &mut converted, &mut BTreeSet::new())?;
    }
    tx.execute_batch(
        "ALTER TABLE cas ADD COLUMN codec INTEGER NOT NULL DEFAULT 85 CHECK(codec IN (85,113));",
    )?;
    for (old, new) in &converted {
        let object = &objects[old];
        tx.execute(
            "INSERT OR IGNORE INTO cas(hash,kind,bytes,created_at,codec) VALUES (?,?,?,?,?)",
            params![new.hash, object.kind, new.bytes, object.created, new.codec],
        )?;
    }
    for (old, new) in &converted {
        for target in [
            Target {
                table: "definition_records",
                column: "event_hash",
            },
            Target {
                table: "defs",
                column: "source_hash",
            },
            Target {
                table: "effect_results",
                column: "result_hash",
            },
        ] {
            let table = target.table;
            let column = target.column;
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?)",
                [table],
                |r| r.get(0),
            )?;
            if exists {
                tx.execute(
                    &format!("UPDATE {table} SET {column}=? WHERE {column}=?"),
                    params![new.hash, old],
                )?;
            }
        }
    }
    for (old, new) in &converted {
        if old != &new.hash {
            tx.execute("DELETE FROM cas WHERE hash=?", [old])?;
        }
    }
    tx.execute_batch("UPDATE defs SET component_hash=NULL;")?;
    register_codecs(&tx)?;
    // Durable replay instruction: old components implement the former wire codec.
    super::record_definition_event(
        &tx,
        &serde_json::json!({"type":"dag_cbor_migrated","version":1}),
    )?;
    let violations: i64 =
        tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })?;
    ensure!(
        violations == 0,
        "DAG-CBOR migration found {violations} broken persisted links; original database retained"
    );
    tx.commit()?;
    Ok(())
}
fn convert(
    hash: &str,
    objects: &BTreeMap<String, Object>,
    output: &mut BTreeMap<String, Converted>,
    visiting: &mut BTreeSet<String>,
) -> Result<()> {
    if output.contains_key(hash) {
        return Ok(());
    }
    ensure!(
        visiting.insert(hash.to_owned()),
        "cyclic legacy CAS reference {hash}"
    );
    let object = objects
        .get(hash)
        .with_context(|| format!("legacy reference {hash} missing from CAS"))?;
    let structured = matches!(
        object.kind.as_str(),
        "event" | "result" | "state" | "message" | "tree" | "desc"
    );
    let bytes = if structured {
        let mut value: Value = serde_json::from_slice(&object.bytes)
            .with_context(|| format!("invalid legacy {} {hash}", object.kind))?;
        if object.kind == "tree" {
            migrate_tree(&mut value, objects, output, visiting)?;
        }
        rewrite(&mut value, objects, output, visiting)?;
        super::encode(&value)?
    } else {
        object.bytes.clone()
    };
    output.insert(
        hash.into(),
        Converted {
            hash: blake3::hash(&bytes).to_hex().to_string(),
            codec: if structured { 113 } else { 85 },
            bytes,
        },
    );
    visiting.remove(hash);
    Ok(())
}
fn rewrite(
    value: &mut Value,
    objects: &BTreeMap<String, Object>,
    output: &mut BTreeMap<String, Converted>,
    visiting: &mut BTreeSet<String>,
) -> Result<()> {
    match value {
        Value::Object(map) => {
            if map.contains_key("$ref") {
                ensure!(
                    map.len() == 1,
                    "legacy $ref contains extra keys; reconcile before migration"
                );
                let hash = map["$ref"]
                    .as_str()
                    .context("legacy ref is not a string")?
                    .to_owned();
                if loom_proto::parse_reference(&hash).is_ok() {
                    return Ok(());
                }
                convert(&hash, objects, output, visiting)?;
                let converted = &output[&hash];
                *value = loom_proto::reference(&converted.hash, converted.codec)
                    .map_err(anyhow::Error::msg)?;
            } else {
                for child in map.values_mut() {
                    rewrite(child, objects, output, visiting)?;
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                rewrite(child, objects, output, visiting)?;
            }
        }
        Value::String(hash)
            if objects.contains_key(hash)
                && matches!(
                    objects[hash].kind.as_str(),
                    "event" | "state" | "message" | "result" | "tree" | "desc"
                ) =>
        {
            convert(hash, objects, output, visiting)?;
            *hash = output[hash].hash.clone();
        }
        _ => {}
    }
    Ok(())
}

fn migrate_tree(
    value: &mut Value,
    objects: &BTreeMap<String, Object>,
    output: &mut BTreeMap<String, Converted>,
    visiting: &mut BTreeSet<String>,
) -> Result<()> {
    let entries = value["entries"]
        .as_array_mut()
        .context("legacy tree missing entries")?;
    for entry in entries {
        let map = entry
            .as_object_mut()
            .context("legacy tree entry not object")?;
        let hash = map
            .remove("hash")
            .context("legacy tree entry missing hash")?;
        let hash = hash.as_str().context("legacy tree hash not string")?;
        convert(hash, objects, output, visiting)?;
        let target = &output[hash];
        map.insert(
            "reference".into(),
            loom_proto::reference(&target.hash, target.codec).map_err(anyhow::Error::msg)?,
        );
    }
    Ok(())
}

fn register_codecs(c: &Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS cas_codecs(hash TEXT NOT NULL REFERENCES cas(hash) ON DELETE CASCADE,codec INTEGER NOT NULL CHECK(codec IN (85,113)),PRIMARY KEY(hash,codec)); INSERT OR IGNORE INTO cas_codecs SELECT hash,codec FROM cas;")?;
    Ok(())
}
