//! Transactional conversion of the legacy JSON CAS. No legacy codec remains live.
use anyhow::{Context, Result, ensure};
use loom_proto::{Event, Value};
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
    let pending: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='inbox')",
        [],
        |r| r.get(0),
    )?;
    if pending {
        let count: i64 = tx.query_row("SELECT count(*) FROM inbox", [], |r| r.get(0))?;
        ensure!(
            count == 0,
            "DAG-CBOR migration refused: pending legacy inbox messages may resume effects with changed descriptor identities; drain messages with the legacy daemon before migration"
        );
    }
    let mut archived = Vec::new();
    for object in objects.values() {
        if object.kind == "event_archive" {
            let bytes = zstd::stream::decode_all(object.bytes.as_slice())?;
            archived.extend(serde_json::from_slice::<Vec<Event>>(&bytes)?);
        }
        if object.kind == "event" {
            let event: Value = serde_json::from_slice(&object.bytes)?;
            ensure!(
                event["type"] != "effect_recorded",
                "DAG-CBOR migration refused: legacy effect descriptor preimages were not persisted; retain the original database and export/reconcile effects before migration"
            );
        }
    }
    ensure!(
        !archived
            .iter()
            .any(|e| e.event["type"] == "effect_recorded"),
        "DAG-CBOR migration refused: archived legacy effects lack descriptor preimages; retain original database and reconcile effects"
    );
    let effects: i64 = tx.query_row("SELECT count(*) FROM effect_results", [], |r| r.get(0))?;
    ensure!(
        effects == 0,
        "DAG-CBOR migration refused: legacy effect descriptor preimages unavailable; retain original database and reconcile effects"
    );
    // Expand old archive segments before converting. Subsequent compaction uses DAG-CBOR.
    for event in archived {
        let bytes = serde_json::to_vec(&event.event)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        objects.entry(hash.clone()).or_insert(Object {
            kind: "event".into(),
            bytes: bytes.clone(),
            created: event.ts,
        });
        tx.execute(
            "INSERT OR IGNORE INTO cas VALUES (?,?,?,?)",
            params![hash, "event", bytes, event.ts],
        )?;
        tx.execute(
            "UPDATE log SET actor=?,event_hash=?,handler_seq=?,ts=? WHERE seq=?",
            params![event.actor, hash, event.handler_seq, event.ts, event.seq],
        )?;
    }
    for table in ["archive_entries", "archive_segments"] {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?)",
            [table],
            |r| r.get(0),
        )?;
        if exists {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
    }
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
                table: "log",
                column: "event_hash",
            },
            Target {
                table: "defs",
                column: "source_hash",
            },
            Target {
                table: "snapshots",
                column: "state_hash",
            },
            Target {
                table: "effect_results",
                column: "result_hash",
            },
            Target {
                table: "message_keys",
                column: "msg_hash",
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
    tx.execute_batch(
        "UPDATE defs SET component_hash=NULL; UPDATE actors SET component_hash=NULL;",
    )?;
    register_codecs(&tx)?;
    // Durable replay instruction: old components implement the former wire codec.
    super::append(
        &tx,
        "system",
        &serde_json::json!({"type":"dag_cbor_migrated","version":1}),
        0,
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
    let bytes = if object.kind == "event_archive" {
        let bytes = zstd::stream::decode_all(object.bytes.as_slice())?;
        let mut value: Value = serde_json::from_slice(&bytes)?;
        rewrite(&mut value, objects, output, visiting)?;
        zstd::stream::encode_all(super::encode(&value)?.as_slice(), 3)?
    } else if structured {
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
