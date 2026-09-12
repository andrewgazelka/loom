use loom_api::Service;
use loom_proto::{CommandRequest, Lang};
use loom_store::Store;
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf};

fn counts(store: &Store) -> anyhow::Result<BTreeMap<String, i64>> {
    store.with_connection(|connection| {
        let names = connection
            .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut counts = BTreeMap::new();
        for name in names {
            let query = format!("SELECT count(*) FROM \"{}\"", name.replace('"', "\"\""));
            counts.insert(name, connection.query_row(&query, [], |row| row.get(0))?);
        }
        Ok(counts)
    })
}

fn service() -> anyhow::Result<Service> {
    Service::new(
        Store::memory()?,
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )
}

async fn assert_unchanged(service: Service, source: &str, expected: &str) -> anyhow::Result<()> {
    let before = counts(&service.store)?;
    let response = service
        .command(CommandRequest {
            session: None,
            command: "add".into(),
            args: json!({"name":"failed-intake","source":source}),
        })
        .await;
    assert!(!response.ok, "{response:?}");
    assert!(format!("{response:?}").contains(expected), "{response:?}");
    let after = counts(&service.store)?;
    println!(
        "store rows before={} after={}; before={before:?}; after={after:?}",
        before.values().sum::<i64>(),
        after.values().sum::<i64>()
    );
    assert_eq!(before, after, "failed intake must not add tables or rows");
    Ok(())
}

#[tokio::test]
async fn missing_driver_add_preserves_all_store_rows() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("nonexistent-hash-rustc");
    assert_unchanged(
        service()?.with_driver_path(path.clone()),
        "pub fn main() -> i32 { 42 }",
        path.to_str().unwrap(),
    )
    .await
}

#[tokio::test]
async fn compile_error_add_preserves_all_store_rows() -> anyhow::Result<()> {
    assert_unchanged(
        service()?,
        "pub fn main() -> i32 { true }",
        "mismatched types",
    )
    .await
}
