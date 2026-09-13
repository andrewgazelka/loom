//! Read-only cross-actor inspection through the capability replay boundary.
use crate::{Cap, ChildState, Ctx, EffectError, EffectKey, Node, Rights, SqlValue, Trap, actor};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Inspection {
    pub columns: Vec<String>,
    pub rows: Vec<InspectionRow>,
}
#[derive(Serialize, Deserialize)]
pub struct InspectionRow {
    pub values: Vec<SqlValue>,
}

impl Ctx<'_> {
    pub async fn inspect_sql(&mut self, cap: &Cap, query: &str, params: Vec<turso::Value>) -> Result<Inspection, Trap> {
        let bytes = self
            .cap_operation(crate::cap_ops::Operation::InspectSql {
                cap: cap.clone(),
                query: query.into(),
                params: params.into_iter().map(SqlValue::capture).collect(),
            })
            .await?;
        serde_json::from_slice(&bytes).map_err(|e| self.runtime(e))
    }
}

async fn state_on(conn: &turso::Connection, cap: &Cap) -> Result<ChildState> {
    let code = actor::code(conn).await?;
    let poison = actor::query(conn, "SELECT value FROM meta WHERE key='poison_revision'", ()).await?;
    Ok(ChildState {
        id: cap.target.clone(),
        status: actor::status(conn).await?,
        behavior_hash: code.hash,
        generation: actor::meta(conn, "generation").await?.parse()?,
        revision: code.revision,
        poison_revision: poison.rows.first().map(|r| r.get::<String>(0)).transpose()?.map(|v| v.parse()).transpose()?,
    })
}

pub(crate) async fn state(node: &Node, current: &turso::Connection, key: &EffectKey, cap: &Cap) -> Result<Vec<u8>, EffectError> {
    node.verify_cap_mac(cap, Rights::INSPECT, "inspect")?;
    let reader;
    let conn = if cap.target == key.actor_id {
        current
    } else {
        // A driver cap's authority is its owning actor (same redirection as capability.rs).
        let authority = if cap.target.starts_with("drv:") { crate::drivers::target(&cap.target).map_err(EffectError::Deterministic)?.owner } else { cap.target.as_str() };
        reader = node.capability_reader(authority).await?;
        &reader
    };
    node.verify_cap_on(conn, cap, Rights::INSPECT, "inspect").await?;
    let state = state_on(conn, cap).await.map_err(EffectError::Environmental)?;
    serde_json::to_vec(&state).map_err(|e| EffectError::Environmental(e.into()))
}

pub(crate) async fn query(
    node: &Node,
    current: &turso::Connection,
    key: &EffectKey,
    cap: &Cap,
    sql: &str,
    params: Vec<SqlValue>,
) -> Result<Vec<u8>, EffectError> {
    node.verify_cap_mac(cap, Rights::INSPECT, "inspect_sql")?;
    let reader;
    let conn = if cap.target == key.actor_id {
        current
    } else {
        let authority = if cap.target.starts_with("drv:") { crate::drivers::target(&cap.target).map_err(EffectError::Deterministic)?.owner } else { cap.target.as_str() };
        reader = node.capability_reader(authority).await?;
        &reader
    };
    node.verify_cap_on(conn, cap, Rights::INSPECT, "inspect_sql").await?;
    actor::inspect_statement(sql)
        .map_err(|error| EffectError::Deterministic(error.context(format!("inspect_sql cap_id {}", cap.cap_id))))?;
    let result = async {
        let params: Vec<turso::Value> = params.iter().map(SqlValue::value).collect();
        let result = actor::query(conn, sql, params).await?;
        let mut rows = Vec::new();
        for row in result.rows {
            let mut values = Vec::new();
            for index in 0..row.column_count() {
                values.push(SqlValue::capture(row.get_value(index)?));
            }
            rows.push(InspectionRow { values });
        }
        serde_json::to_vec(&Inspection { columns: result.columns, rows }).context("serialize inspection")
    }
    .await;
    result.map_err(|error| EffectError::Environmental(error.context(format!("inspect_sql cap_id {}", cap.cap_id))))
}
