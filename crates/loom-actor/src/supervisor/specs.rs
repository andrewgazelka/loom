use super::{SpecRow, cell, error, integer, text};
use crate::{ChildSpec, ChildType, Ctx, RestartPolicy, Trap, Value};

pub(super) async fn specs(cx: &mut Ctx<'_>) -> Result<Vec<SpecRow>, Trap> {
    let rows = cx
        .sql("SELECT child_id,\"order\",behavior_hash,init,restart,shutdown,link,type,monitor,durability FROM spec ORDER BY \"order\"", ())
        .await?;
    let mut result = Vec::new();
    for row in rows.rows {
        let init = match cell(cx, &row, 3)? {
            Value::Blob(value) => value,
            _ => return Err(error(cx, "child init must be a blob")),
        };
        let restart = match text(cx, &row, 4)?.as_str() {
            "permanent" => RestartPolicy::Permanent,
            "transient" => RestartPolicy::Transient,
            "temporary" => RestartPolicy::Temporary,
            _ => return Err(error(cx, "invalid child restart policy")),
        };
        let shutdown = serde_json::from_str(&text(cx, &row, 5)?).map_err(|e| error(cx, e))?;
        let link = match integer(cx, &row, 6)? {
            0 => false,
            1 => true,
            _ => return Err(error(cx, "invalid child link flag")),
        };
        result.push(SpecRow {
            id: text(cx, &row, 0)?,
            order: integer(cx, &row, 1)?,
            spec: ChildSpec {
                durability: match text(cx, &row, 9)?.as_str() {
                    "local" => crate::Durability::Local,
                    "remote" => crate::Durability::Remote,
                    _ => return Err(error(cx, "invalid child durability")),
                },
                behavior_hash: text(cx, &row, 2)?,
                init,
                restart,
                shutdown,
                link,
                child_type: match text(cx, &row, 7)?.as_str() {
                    "worker" => ChildType::Worker,
                    "supervisor" => ChildType::Supervisor,
                    _ => return Err(error(cx, "invalid child type")),
                },
                monitor: match integer(cx, &row, 8)? {
                    0 => false,
                    1 => true,
                    _ => return Err(error(cx, "invalid child monitor flag")),
                },
            },
        });
    }
    Ok(result)
}
