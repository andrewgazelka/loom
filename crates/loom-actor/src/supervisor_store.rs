use crate::{Cap, ChildSpec, ChildType, RestartPolicy};

pub(crate) async fn record_child(conn: &turso::Connection, cap: &Cap, spec: &ChildSpec) -> anyhow::Result<()> {
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
    conn.execute("INSERT INTO spec(child_id,\"order\",behavior_hash,init,restart,shutdown,link,type,monitor,durability,cap) SELECT ?,COALESCE(MAX(\"order\"),0)+1,?,?,?,?,?,?,?,?,? FROM spec",
        turso::params![cap.target.as_str(), spec.behavior_hash.as_str(), spec.init.as_slice(), restart, shutdown, spec.link, child_type, spec.monitor, spec.durability.name(), serde_json::to_string(cap)?]).await?;
    Ok(())
}

/// Host API child registration is outside mailbox replay, so it carries an
/// explicit journal entry at the parent's commit boundary.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct HostSpawn {
    pub(crate) epoch: i64,
    pub(crate) child_id: String,
    pub(crate) cap: Cap,
    pub(crate) spec: ChildSpec,
}

pub(crate) async fn record_host_spawn(conn: &turso::Connection, cap: &Cap, spec: &ChildSpec) -> anyhow::Result<()> {
    use anyhow::Context;
    let rows = crate::actor::query(conn, "SELECT value FROM meta WHERE key='host_spawn_counter'", ()).await?;
    let counter = match rows.rows.first() {
        Some(row) => row.get::<String>(0)?.parse::<i64>()?,
        None => 0,
    }
    .checked_add(1)
    .context("host spawn counter overflow")?;
    let operation = HostSpawn {
        epoch: crate::actor::meta(conn, "commit_epoch").await?.parse()?,
        child_id: cap.target.clone(),
        cap: cap.clone(),
        spec: spec.clone(),
    };
    crate::actor::set_meta(conn, &format!("host_spawn:{counter}"), &serde_json::to_string(&operation)?).await?;
    crate::actor::set_meta(conn, "host_spawn_counter", &counter.to_string()).await
}

pub(crate) async fn replay_host_spawns(source: &turso::Connection, target: &mut turso::Connection, epoch: i64) -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    struct Entry {
        counter: i64,
        key: String,
        value: String,
        operation: HostSpawn,
    }
    let rows = crate::actor::query(source, "SELECT key,value FROM meta WHERE key LIKE 'host_spawn:%'", ()).await?;
    let mut entries = Vec::new();
    for row in rows.rows {
        let key: String = row.get(0)?;
        let counter: i64 = key.strip_prefix("host_spawn:").context("invalid host spawn journal key")?.parse()?;
        ensure!(counter > 0, "invalid host spawn counter {counter}");
        let value: String = row.get(1)?;
        entries.push(Entry { counter, key, operation: serde_json::from_str(&value)?, value });
    }
    entries.sort_by_key(|entry| entry.counter);
    for entry in entries {
        if entry.operation.epoch > epoch {
            continue;
        }
        let existing = crate::actor::query(target, "SELECT value FROM meta WHERE key=?", [entry.key.as_str()]).await?;
        if let Some(row) = existing.rows.first() {
            ensure!(row.get::<String>(0)? == entry.value, "host spawn journal differs at {}", entry.key);
            continue;
        }
        let tx = target.transaction().await?;
        record_child(&tx, &entry.operation.cap, &entry.operation.spec).await?;
        crate::actor::set_meta(&tx, &entry.key, &entry.value).await?;
        crate::actor::set_meta(&tx, "host_spawn_counter", &entry.counter.to_string()).await?;
        tx.commit().await?;
    }
    Ok(())
}
