//! Destructive cache control for an isolated benchmark database, never a daemon RPC.
use loom_store::Store;

fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let database = arguments
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: evict_artifact ISOLATED_DB CRATE_NAME"))?;
    let name = arguments
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing crate name"))?;
    anyhow::ensure!(arguments.next().is_none(), "unexpected argument");
    let store = Store::open(database)?;
    let manifests = store.with_connection(|connection| {
        let mut statement =
            connection.prepare("SELECT artifact_hash FROM rust_artifacts ORDER BY rowid DESC")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    })?;
    for hash in manifests {
        let artifact: serde_json::Value = store
            .get_value(&hash)?
            .ok_or_else(|| anyhow::anyhow!("artifact manifest missing"))?;
        if artifact["name"].as_str() != Some(&name) {
            continue;
        }
        let outputs = artifact["outputs"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("artifact outputs absent"))?;
        let mut removed = 0;
        for output in outputs {
            let path = output["path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("artifact path absent"))?;
            let hash = output["hash"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("artifact hash absent"))?;
            if std::path::Path::new(path).exists() {
                std::fs::remove_file(path)?;
            }
            store.with_connection(|connection| {
                connection.execute("DELETE FROM cas WHERE hash=?", [hash])?;
                Ok(())
            })?;
            anyhow::ensure!(
                store.get(hash)?.is_none(),
                "CAS eviction did not take effect"
            );
            removed += 1;
        }
        anyhow::ensure!(removed > 0, "crate has no artifacts");
        println!(
            "{}",
            serde_json::json!({"name":name,"key":artifact["key"],"removed_outputs":removed})
        );
        return Ok(());
    }
    anyhow::bail!("compiled crate {name} not found")
}
