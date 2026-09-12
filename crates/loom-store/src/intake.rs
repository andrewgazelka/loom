//! Build intake writes stay private until a definition can be published.
use super::*;

pub struct IntakePublication<'a> {
    pub def: &'a Def,
    pub name: Option<&'a str>,
    pub source: &'a str,
    pub deps: &'a BTreeMap<String, String>,
    pub identity: Option<&'a loom_proto::BuildIdentity>,
    pub build_event: &'a Value,
}

impl Store {
    /// Snapshot committed state without holding a live transaction during compilation.
    pub fn stage_intake(&self) -> Result<Self> {
        self.recording.barrier(false)?;
        let mut snapshot = Connection::open_in_memory()?;
        {
            let source = self.lock()?;
            let backup = rusqlite::backup::Backup::new(&source, &mut snapshot)?;
            backup.run_to_completion(256, std::time::Duration::from_millis(1), None)?;
        }
        Self::initialize(snapshot, recording::Durability::Ephemeral)
    }

    /// Import build objects and publish against current state in one transaction.
    /// Live names, records and effects are never replaced by snapshot projections.
    pub fn commit_intake(&self, staged: &Store, publication: IntakePublication<'_>) -> Result<i64> {
        ensure!(
            !Arc::ptr_eq(&self.connection, &staged.connection),
            "intake requires a separate staged store"
        );
        staged.recording.barrier(false)?;
        self.recording.barrier(false)?;
        let source = staged.lock()?;
        let mut destination = self.lock()?;
        let tx = destination.transaction()?;
        copy_rows(&source, &tx, "cas", "hash,kind,bytes,created_at,codec")?;
        copy_rows(&source, &tx, "cas_codecs", "hash,codec")?;
        import_caches(&source, &tx)?;
        let seq = publication::write(
            &tx,
            publication.def,
            publication.name,
            publication.source,
            publication.deps,
            publication.identity,
        )?;
        ensure!(
            publication.build_event["type"] == "component_built",
            "intake publication requires a component_built event"
        );
        record_definition_event(&tx, publication.build_event)?;
        tx.commit()?;
        Ok(seq)
    }
}

struct CacheTable {
    name: &'static str,
    columns: &'static str,
    schema: &'static str,
}

fn import_caches(source: &Connection, destination: &Connection) -> Result<()> {
    // Match the compiler cache policy transition without importing obsolete keys.
    if table_exists(source, "rust_artifact_policy")?
        && !table_exists(destination, "rust_artifact_policy")?
        && table_exists(destination, "rust_artifacts")?
    {
        destination.execute("DELETE FROM rust_artifacts", [])?;
    }
    for table in [
        CacheTable {
            name: "rust_preparations",
            columns: "key,overlay_hash",
            schema: "key TEXT PRIMARY KEY, overlay_hash TEXT NOT NULL",
        },
        CacheTable {
            name: "rust_artifacts",
            columns: "key,artifact_hash",
            schema: "key TEXT PRIMARY KEY, artifact_hash TEXT NOT NULL",
        },
        CacheTable {
            name: "rust_artifact_policy",
            columns: "version",
            schema: "version INTEGER PRIMARY KEY",
        },
        CacheTable {
            name: "rust_build_graphs",
            columns: "key,recipe_hash",
            schema: "key TEXT PRIMARY KEY, recipe_hash TEXT NOT NULL",
        },
    ] {
        if table_exists(source, table.name)? {
            destination.execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS {} ({})",
                table.name, table.schema
            ))?;
            copy_rows(source, destination, table.name, table.columns)?;
        }
    }
    Ok(())
}

fn table_exists(connection: &Connection, name: &str) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
        [name],
        |row| row.get(0),
    )?)
}

fn copy_rows(
    source: &Connection,
    destination: &Connection,
    table: &str,
    columns: &str,
) -> Result<()> {
    let mut query = source.prepare(&format!("SELECT {columns} FROM {table}"))?;
    let count = query.column_count();
    let placeholders = vec!["?"; count].join(",");
    let mut insert = destination.prepare(&format!(
        "INSERT OR IGNORE INTO {table} ({columns}) VALUES ({placeholders})"
    ))?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let values = (0..count)
            .map(|index| row.get::<_, rusqlite::types::Value>(index))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        insert.execute(rusqlite::params_from_iter(values))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn candidate() -> Def {
        Def {
            hash: blake3::hash(b"intake definition").to_hex().to_string(),
            lang: loom_proto::Lang::Rust,
            component_hash: None,
            sig: Default::default(),
            allowed_effects: None,
            observed_effects: Vec::new(),
        }
    }

    #[test]
    fn publication_failure_rolls_back_objects_caches_and_events() -> Result<()> {
        let live = Store::memory()?;
        let staged = live.stage_intake()?;
        let object = staged.put("test", b"private build output")?;
        staged.with_connection(|connection| {
            connection.execute_batch("CREATE TABLE rust_preparations(key TEXT PRIMARY KEY, overlay_hash TEXT NOT NULL); INSERT INTO rust_preparations VALUES ('private','overlay')")?;
            Ok(())
        })?;
        let before = live.latest_seq()?;
        let result = live.commit_intake(
            &staged,
            IntakePublication {
                def: &candidate(),
                name: Some("private"),
                source: "source",
                deps: &BTreeMap::new(),
                identity: None,
                // Fail after CAS import and definition projection to prove rollback.
                build_event: &json!({"type":"invalid"}),
            },
        );
        assert!(result.is_err());
        assert_eq!(live.latest_seq()?, before);
        assert!(live.resolve("private")?.is_none());
        live.with_connection(|connection| {
            let imported: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM cas WHERE hash=?)",
                [&object],
                |row| row.get(0),
            )?;
            let table: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='rust_preparations')",
                [],
                |row| row.get(0),
            )?;
            assert!(!imported);
            assert!(!table);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn successful_intake_preserves_live_effects_and_names() -> Result<()> {
        let live = Store::memory()?;
        let staged = live.stage_intake()?;
        live.effect_put("live effect", "scope", 0, &json!(42))?;
        let mut concurrent = candidate();
        concurrent.hash = blake3::hash(b"concurrent").to_hex().to_string();
        live.define(&concurrent, Some("concurrent"), "other", &BTreeMap::new())?;
        let before = live.latest_seq()?;
        let def = candidate();
        let seq = live.commit_intake(
            &staged,
            IntakePublication {
                def: &def,
                name: Some("built"),
                source: "source",
                deps: &BTreeMap::new(),
                identity: None,
                build_event: &json!({"type":"component_built"}),
            },
        )?;
        assert!(seq > before);
        assert_eq!(live.latest_seq()?, seq + 1);
        assert_eq!(live.resolve("built")?.unwrap().hash, def.hash);
        assert_eq!(live.resolve("concurrent")?.unwrap().hash, concurrent.hash);
        assert_eq!(live.effect_get("live effect", "scope", 0)?, Some(json!(42)));
        Ok(())
    }
}
