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

pub fn rows(cx: &mut Ctx<'_>, rows: Rows) -> Result<Value, Trap> {
    #[derive(Serialize)]
    struct ResultRows {
        columns: Vec<String>,
        rows: Vec<Vec<Cell>>,
    }
    let mut output = Vec::new();
    for row in rows.rows {
        let mut cells = Vec::new();
        for index in 0..rows.columns.len() {
            let cell = match row
                .get_value(index)
                .map_err(|error| cx.runtime(format!("sql result: {error}")))?
            {
                SqlValue::Null => Cell::Null,
                SqlValue::Integer(value) => Cell::Integer(value),
                SqlValue::Real(value) if value.is_finite() => Cell::Real(value),
                SqlValue::Real(_) => return Err(Trap::new("sql result: non-finite real")),
                SqlValue::Text(value) => Cell::Text(value),
                SqlValue::Blob(value) => Cell::Blob(value),
            };
            cells.push(cell);
        }
        output.push(cells);
    }
    value(ResultRows {
        columns: rows.columns,
        rows: output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
