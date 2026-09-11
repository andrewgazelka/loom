use crate::{ChildSpec, ChildType, RestartPolicy};

pub(crate) async fn record_child(conn: &turso::Connection, id: &str, spec: &ChildSpec) -> anyhow::Result<()> {
    let restart = match spec.restart {
        RestartPolicy::Permanent => "permanent",
        RestartPolicy::Transient => "transient",
        RestartPolicy::Temporary => "temporary",
    };
    let child_type = match &spec.child_type {
        ChildType::Worker => "worker",
        ChildType::Supervisor => "supervisor",
    };
    let shutdown = serde_json::to_string(&spec.shutdown)?;
    conn.execute("INSERT INTO spec(child_id,\"order\",behavior_hash,init,restart,shutdown,link,type,monitor) SELECT ?,COALESCE(MAX(\"order\"),0)+1,?,?,?,?,?,?,? FROM spec",
        turso::params![id, spec.behavior_hash.as_str(), spec.init.as_slice(), restart, shutdown, spec.link, child_type, spec.monitor]).await?;
    Ok(())
}
