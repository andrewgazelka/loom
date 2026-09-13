//! Decode the engine's SQLite record format using the same engine's decoder.
use crate::actor;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use turso_core::{Numeric, ValueRef, types::ValueIterator};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeltaRow {
    pub change_id: i64,
    pub change_type: i64,
    pub table: String,
    pub id: Value,
    pub before: Value,
    pub after: Value,
    pub updates: Value,
}

pub(crate) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(crate) fn json_cell(value: turso::Value) -> Result<Value> {
    Ok(match value {
        turso::Value::Null => Value::Null,
        turso::Value::Integer(value) => json!(value),
        turso::Value::Real(value) => {
            ensure!(value.is_finite(), "CDC refuses non-finite JSON real");
            json!(value)
        }
        turso::Value::Text(value) => json!(value),
        turso::Value::Blob(value) => json!(value),
    })
}

pub fn decode_record(bytes: &[u8]) -> Result<Vec<turso::Value>> {
    ValueIterator::new(bytes)?.map(|value| Ok(match value? {
        ValueRef::Null => turso::Value::Null,
        ValueRef::Numeric(Numeric::Integer(value)) => turso::Value::Integer(value),
        ValueRef::Numeric(Numeric::Float(value)) => turso::Value::Real(value.into()),
        ValueRef::Text(value) => turso::Value::Text(value.as_str().to_owned()),
        ValueRef::Blob(value) => turso::Value::Blob(value.to_vec()),
    })).collect()
}

async fn object(conn: &turso::Connection, table: &str, record: turso::Value) -> Result<Value> {
    let bytes = match record {
        turso::Value::Null => return Ok(Value::Null),
        turso::Value::Blob(bytes) => bytes,
        _ => anyhow::bail!("CDC {table}: expected binary record"),
    };
    let columns = actor::query(conn, &format!("SELECT * FROM {} LIMIT 0", quote(table)), ()).await?.columns;
    let values = decode_record(&bytes)?;
    ensure!(columns.len() == values.len(), "CDC {table}: record/schema width differs");
    let mut object = serde_json::Map::new();
    for (column, value) in columns.into_iter().zip(values) {
        object.insert(column, json_cell(value)?);
    }
    Ok(Value::Object(object))
}

pub(crate) async fn delta(conn: &turso::Connection, row: &turso::Row) -> Result<DeltaRow> {
    let table: String = row.get(2)?;
    Ok(DeltaRow {
        change_id: row.get(0)?, change_type: row.get(1)?, id: json_cell(row.get_value(3)?)?,
        before: object(conn, &table, row.get_value(4)?).await?,
        after: object(conn, &table, row.get_value(5)?).await?,
        // Updates is the engine's sparse binary record, preserved as bytes.
        updates: json_cell(row.get_value(6)?)?, table,
    })
}

pub(crate) async fn high_water(conn: &turso::Connection) -> Result<i64> {
    actor::query(conn, "SELECT COALESCE(MAX(change_id),0) FROM turso_cdc", ()).await?
        .rows.first().context("CDC missing high-water result")?.get(0).map_err(Into::into)
}

/// Apply engine row images to a snapshot with CDC disabled on the destination.
/// Used independently of behavior replay to check invariant 17.
pub async fn replay_domain_cdc(source: &turso::Connection, target: &turso::Connection, after_change_id: i64) -> Result<()> {
    let id = actor::meta(source, "id").await?;
    let seq = actor::cursor(source).await?;
    replay_without_triggers(source, target, after_change_id).await
        .with_context(|| format!("actor {id} seq {seq}: replay domain CDC"))
}

async fn replay_without_triggers(source: &turso::Connection, target: &turso::Connection, after_change_id: i64) -> Result<()> {
    struct Trigger {
        name: String,
        sql: String,
    }
    let definitions = actor::query(target, "SELECT name,sql FROM sqlite_schema WHERE type='trigger' ORDER BY rowid", ()).await?;
    let mut triggers = Vec::new();
    for row in definitions.rows {
        triggers.push(Trigger { name: row.get(0)?, sql: row.get(1)? });
    }
    ensure!(triggers.is_empty() || !target.is_autocommit()?, "CDC replay with triggers requires a caller-owned transaction");
    // CDC already includes trigger-produced changes. Replaying their causes with
    // triggers enabled would produce those changes twice. The restore loop leaves
    // this temporary schema state; on any error the caller rolls back its transaction.
    for trigger in &triggers {
        target.execute(format!("DROP TRIGGER {}", quote(&trigger.name)), ()).await
            .with_context(|| format!("suspend replay trigger {}", trigger.name))?;
    }
    replay_rows(source, target, after_change_id).await?;
    for trigger in triggers {
        target.execute(&trigger.sql, ()).await.with_context(|| format!("restore replay trigger {}", trigger.name))?;
    }
    Ok(())
}

async fn replay_rows(source: &turso::Connection, target: &turso::Connection, after_change_id: i64) -> Result<()> {
    let rows = actor::query(source,
        "SELECT change_type,table_name,id,after FROM turso_cdc WHERE change_id>? AND change_type!=2 ORDER BY change_id",
        [after_change_id]).await?;
    for row in rows.rows {
        let table: String = row.get(1)?;
        if crate::schema::SYSTEM_TABLES.contains(&table.as_str()) || table.starts_with("sqlite_") || table.starts_with("turso_") {
            continue;
        }
        let kind: i64 = row.get(0)?;
        let id = row.get_value(2)?;
        if kind == -1 || kind == 0 {
            target.execute(format!("DELETE FROM {} WHERE rowid=?", quote(&table)), vec![id.clone()]).await?;
        }
        if kind == 1 || kind == 0 {
            let bytes: Vec<u8> = row.get(3)?;
            let mut values = vec![id];
            values.extend(decode_record(&bytes)?);
            let columns = actor::query(target, &format!("SELECT * FROM {} LIMIT 0", quote(&table)), ()).await?.columns;
            ensure!(columns.len() + 1 == values.len(), "CDC {table}: replay schema width differs");
            let names = columns.iter().map(|name| quote(name)).collect::<Vec<_>>().join(",");
            let sql = format!("INSERT INTO {} (rowid,{names}) VALUES ({})", quote(&table), vec!["?"; values.len()].join(","));
            target.execute(sql, values).await?;
        } else {
            ensure!(kind == -1, "CDC {table}: unknown change_type {kind}");
        }
    }
    Ok(())
}

pub(crate) async fn snapshot(conn: &turso::Connection, table: &str) -> Result<Vec<Value>> {
    let rows = actor::query(conn, &format!("SELECT rowid,* FROM {} ORDER BY rowid", quote(table)), ()).await?;
    let mut result = Vec::new();
    for row in rows.rows {
        let mut object = serde_json::Map::new();
        for (index, column) in rows.columns.iter().enumerate().skip(1) {
            object.insert(column.clone(), json_cell(row.get_value(index)?)?);
        }
        result.push(json!({"id":json_cell(row.get_value(0)?)?,"after":object}));
    }
    Ok(result)
}
