use loom_actor::{ChildSpec, Ctx, Rows, Trap, Value as SqlValue};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub op: String,
    #[serde(default)]
    pub args: Value,
}

/// The descriptor carries opaque UTF-8 JSON token bytes, never an actor ID.
#[derive(Deserialize)]
#[serde(try_from = "Vec<u8>")]
pub struct Capability {
    pub token: loom_actor::Cap,
}
impl TryFrom<Vec<u8>> for Capability {
    type Error = serde_json::Error;
    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        Ok(Self {
            token: serde_json::from_slice(&bytes)?,
        })
    }
}
pub fn cap_id(op: &str, decimal: &str) -> Result<u64, Trap> {
    decimal
        .parse()
        .map_err(|error| Trap::new(format!("effect {op}: invalid cap_id {decimal:?}: {error}")))
}

pub fn capability_value(cap: loom_actor::Cap) -> Result<Value, Trap> {
    let bytes = serde_json::to_vec(&cap)
        .map_err(|error| Trap::new(format!("capability result: {error}")))?;
    value(bytes)
}

#[derive(Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Short,
    Request,
}

pub fn parse<T: DeserializeOwned>(op: &str, value: Value) -> Result<T, Trap> {
    serde_json::from_value(value).map_err(|error| Trap::new(format!("effect {op}: {error}")))
}
pub fn value(value: impl Serialize) -> Result<Value, Trap> {
    serde_json::to_value(value).map_err(|error| Trap::new(error.to_string()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sql {
    pub sql: String,
    pub params: Vec<Cell>,
}
#[derive(Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}
impl From<Cell> for SqlValue {
    fn from(cell: Cell) -> Self {
        match cell {
            Cell::Null => Self::Null,
            Cell::Integer(value) => Self::Integer(value),
            Cell::Real(value) => Self::Real(value),
            Cell::Text(value) => Self::Text(value),
            Cell::Blob(value) => Self::Blob(value),
        }
    }
}
pub fn child_spec(args: Value) -> Result<ChildSpec, Trap> {
    let spec: ChildSpec = parse("actor.spawn", args.clone())?;
    // ChildSpec owns its field names and defaults. Reject discarded fields at
    // this wire boundary without duplicating those defaults in the adapter.
    let accepted = value(&spec)?;
    if let Some(fields) = args.as_object() {
        for name in fields.keys() {
            if accepted.get(name).is_none() {
                return Err(Trap::new(format!(
                    "effect actor.spawn: unknown field {name}"
                )));
            }
        }
    }
    Ok(spec)
}

#[derive(Serialize)]
struct ResultRows {
    columns: Vec<String>,
    rows: Vec<Vec<Cell>>,
}

fn cell(value: SqlValue) -> Result<Cell, Trap> {
    Ok(match value {
        SqlValue::Null => Cell::Null,
        SqlValue::Integer(value) => Cell::Integer(value),
        SqlValue::Real(value) if value.is_finite() => Cell::Real(value),
        SqlValue::Real(_) => return Err(Trap::new("sql result: non-finite real")),
        SqlValue::Text(value) => Cell::Text(value),
        SqlValue::Blob(value) => Cell::Blob(value),
    })
}

pub fn rows(cx: &mut Ctx<'_>, rows: Rows) -> Result<Value, Trap> {
    let mut output = Vec::new();
    for row in rows.rows {
        let mut cells = Vec::new();
        for index in 0..rows.columns.len() {
            cells
                .push(cell(row.get_value(index).map_err(|error| {
                    cx.runtime(format!("sql result: {error}"))
                })?)?);
        }
        output.push(cells);
    }
    value(ResultRows {
        columns: rows.columns,
        rows: output,
    })
}

pub fn inspection(rows: loom_actor::Inspection) -> Result<Value, Trap> {
    let output = rows
        .rows
        .into_iter()
        .map(|row| {
            row.values
                .into_iter()
                .map(|value| cell(value.value()))
                .collect::<Result<Vec<_>, Trap>>()
        })
        .collect::<Result<Vec<_>, Trap>>()?;
    value(ResultRows {
        columns: rows.columns,
        rows: output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_wire_preserves_full_width_ids_and_sql_reals() {
        let original = loom_actor::Cap {
            target: "actor".into(),
            cap_id: u64::MAX,
            epoch: u64::MAX,
            rights: loom_actor::Rights::ALL,
            mac: [7; 32],
        };
        let descriptor = serde_json::json!({
            "op": "actor.send", "args": {"cap": capability_value(original.clone()).unwrap(), "msg":[]}
        });
        let encoded = loom_proto::encode(&descriptor).unwrap();
        let decoded: Value = loom_proto::decode(&encoded).unwrap();
        let cap: Capability = parse("actor.send", decoded["args"]["cap"].clone()).unwrap();
        assert_eq!(cap.token, original);
        assert!(loom_proto::encode(&serde_json::json!({"cap_id":u64::MAX})).is_err());
        let id = serde_json::json!({"cap_id":u64::MAX.to_string()});
        let decoded: Value = loom_proto::decode(&loom_proto::encode(&id).unwrap()).unwrap();
        assert_eq!(
            cap_id("actor.revoke", decoded["cap_id"].as_str().unwrap()).unwrap(),
            u64::MAX
        );

        let result = inspection(loom_actor::Inspection {
            columns: vec!["number".into()],
            rows: vec![loom_actor::InspectionRow {
                values: vec![loom_actor::SqlValue::Real(1.5f64.to_bits())],
            }],
        })
        .unwrap();
        let decoded: Value = loom_proto::decode(&loom_proto::encode(&result).unwrap()).unwrap();
        assert_eq!(
            decoded["rows"],
            serde_json::json!([[{"type":"real","value":1.5}]])
        );
    }

    #[test]
    fn spawn_rejects_unknown_fields_and_preserves_child_policy_defaults() {
        let mut args = serde_json::json!({"behavior_hash":"h", "init":[], "type":"supervisor"});
        let spec = child_spec(args.clone()).unwrap();
        assert_eq!(spec.shutdown, loom_actor::Shutdown::Infinity);
        assert!(spec.link);
        assert!(!spec.monitor);
        args["monitr"] = Value::Bool(true);
        let error = child_spec(args).unwrap_err();
        assert!(error.message.contains("actor.spawn"));
        assert!(error.message.contains("monitr"));
    }
}
