use super::*;

impl Store {
    pub fn build_identity(&self, hash: &str) -> Result<Option<loom_proto::BuildIdentity>> {
        Ok(self.lock()?.query_row(
            "SELECT behavior_hash,wasm_hash,toolchain_hash,item_hashes_ref FROM defs WHERE hash=? AND behavior_hash IS NOT NULL",
            [hash], |row| Ok(loom_proto::BuildIdentity {
                behavior_hash: row.get(0)?, wasm_hash: row.get(1)?,
                toolchain_hash: row.get(2)?, item_hashes_ref: row.get(3)?,
            }),
        ).optional()?)
    }
    pub fn revision_timestamp(&self, seq: i64) -> Result<i64> {
        Ok(self.lock()?.query_row(
            "SELECT ts FROM definition_records WHERE seq=?",
            [seq],
            |row| row.get(0),
        )?)
    }
    pub fn current_names(&self) -> Result<BTreeMap<String, String>> {
        let connection = self.lock()?;
        current_names(&connection)
    }
}

pub(super) fn current_names(connection: &Connection) -> Result<BTreeMap<String, String>> {
    let mut query = connection.prepare("SELECT name,hash FROM names n WHERE since_seq=(SELECT MAX(since_seq) FROM names WHERE name=n.name) ORDER BY name")?;
    let mut rows = query.query([])?;
    let mut names = BTreeMap::new();
    while let Some(row) = rows.next()? {
        names.insert(row.get(0)?, row.get(1)?);
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_store_missing_behavior_hash_by_name() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("old.db");
        let connection = Connection::open(&path)?;
        connection.execute_batch("CREATE TABLE defs(hash TEXT,lang TEXT);")?;
        drop(connection);
        let error = Store::open(&path).err().context("old store was accepted")?;
        assert!(
            error
                .to_string()
                .contains("defs missing column behavior_hash"),
            "{error:#}"
        );
        Ok(())
    }
    #[test]
    fn rebuild_preserves_compiler_identity() -> Result<()> {
        let store = Store::memory()?;
        let source = "pub fn main() {}";
        let deps = BTreeMap::new();
        let entries =
            BTreeMap::from([("main".to_owned(), store.put("item-preimage", b"behavior")?)]);
        let root = store.put("entry-root", &loom_proto::entry_identity_preimage(&entries))?;
        let definition = Def {
            hash: root.clone(),
            lang: loom_proto::Lang::Rust,
            component_hash: None,
            sig: Default::default(),
            allowed_effects: None,
            observed_effects: Vec::new(),
        };
        let identity = loom_proto::BuildIdentity {
            behavior_hash: root,
            wasm_hash: store.put("blob", b"wasm")?,
            toolchain_hash: store.put("blob", b"toolchain")?,
            item_hashes_ref: store.put(
                "item-hashes",
                &serde_json::to_vec(&serde_json::json!({"entry":entries}))?,
            )?,
        };
        store.define_with_identity(&definition, Some("main"), source, &deps, Some(&identity))?;
        store.rebuild_views()?;
        let actual = store
            .build_identity(&definition.hash)?
            .context("identity lost during projection rebuild")?;
        assert_eq!(actual.behavior_hash, identity.behavior_hash);
        assert_eq!(actual.wasm_hash, identity.wasm_hash);
        assert_eq!(actual.toolchain_hash, identity.toolchain_hash);
        assert_eq!(actual.item_hashes_ref, identity.item_hashes_ref);
        Ok(())
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;

    fn definition(store: &Store) -> Result<Def> {
        Ok(Def {
            hash: store.put("item-preimage", b"resolved entry")?,
            lang: loom_proto::Lang::Rust,
            component_hash: Some(store.put("component", b"original component")?),
            sig: Default::default(),
            allowed_effects: None,
            observed_effects: Vec::new(),
        })
    }

    #[test]
    fn repeated_identity_keeps_one_name_and_separate_source_revisions() -> Result<()> {
        let store = Store::memory()?;
        let definition = definition(&store)?;
        let first_source = "pub fn main() { let x = 1; x }";
        let second_source = "pub fn main() { let y = 1; y }";
        for source in [first_source, second_source] {
            store.define(&definition, Some("main"), source, &BTreeMap::new())?;
        }
        for rebuilt in [false, true] {
            if rebuilt {
                store.rebuild_views()?;
            }
            assert_eq!(store.name_history("main")?.len(), 1);
            assert_eq!(
                store.source(&definition.hash)?.as_deref(),
                Some(second_source)
            );
            assert_eq!(
                store.get(blake3::hash(first_source.as_bytes()).to_hex().as_str())?,
                Some(first_source.as_bytes().to_vec())
            );
            assert_eq!(
                store.with_connection(|connection| Ok(connection.query_row(
                    "SELECT COUNT(*) FROM source_revisions WHERE def_hash=?",
                    [&definition.hash],
                    |row| row.get::<_, i64>(0)
                )?))?,
                2
            );
        }
        Ok(())
    }

    #[test]
    fn rejects_component_and_schema_republication_without_mutating_original() -> Result<()> {
        let store = Store::memory()?;
        let original = definition(&store)?;
        store.define(&original, Some("main"), "first", &BTreeMap::new())?;
        let seq = store.latest_seq()?;
        let mut candidate = original.clone();
        candidate.component_hash = Some(store.put("component", b"different component")?);
        let error = store
            .define(&candidate, Some("main"), "second", &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(original.component_hash.as_ref().unwrap()),
            "{error}"
        );
        assert!(
            error.contains(candidate.component_hash.as_ref().unwrap()),
            "{error}"
        );
        candidate = original.clone();
        candidate.sig.effects.unknown = false;
        let old_schema = blake3::hash(&serde_json::to_vec(&original.sig)?)
            .to_hex()
            .to_string();
        let new_schema = blake3::hash(&serde_json::to_vec(&candidate.sig)?)
            .to_hex()
            .to_string();
        let error = store
            .define(&candidate, Some("main"), "second", &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&old_schema) && error.contains(&new_schema),
            "{error}"
        );
        assert_eq!(store.latest_seq()?, seq);
        store.rebuild_views()?;
        let actual = store.definition(&original.hash)?.unwrap();
        assert_eq!(actual.component_hash, original.component_hash);
        assert_eq!(actual.sig, original.sig);
        assert_eq!(store.source(&original.hash)?.as_deref(), Some("first"));
        Ok(())
    }
    #[test]
    fn source_revision_cannot_replace_or_union_executable_pins() -> Result<()> {
        let store = Store::memory()?;
        let definition = definition(&store)?;
        let pins = BTreeMap::from([("dependency".into(), "first-hash".into())]);
        store.define(&definition, Some("main"), "first", &pins)?;
        let replacement = BTreeMap::from([("dependency".into(), "other-hash".into())]);
        let seq = store.latest_seq()?;
        let error = store
            .define(&definition, Some("main"), "second", &replacement)
            .unwrap_err();
        assert!(
            error.to_string().contains("dependency pins hash"),
            "{error}"
        );
        assert_eq!(store.latest_seq()?, seq);
        assert_eq!(store.definition_deps(&definition.hash)?, pins);
        assert_eq!(store.dependencies(&definition.hash)?, vec!["first-hash"]);
        store.rebuild_views()?;
        assert_eq!(store.definition_deps(&definition.hash)?, pins);
        assert_eq!(store.dependencies(&definition.hash)?, vec!["first-hash"]);
        Ok(())
    }
    #[test]
    fn reopening_rejects_source_identity_keyed_definition() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("store.db");
        let store = Store::open(&path)?;
        let definition = definition(&store)?;
        store.define(&definition, None, "source", &BTreeMap::new())?;
        let driver_hash = store.put("item-preimage", b"different driver entry")?;
        store.with_connection(|connection| {
            connection.execute(
                "UPDATE defs SET behavior_hash=? WHERE hash=?",
                params![driver_hash, definition.hash],
            )?;
            Ok(())
        })?;
        drop(store);
        let error = Store::open(&path)
            .err()
            .context("old identity store accepted")?
            .to_string();
        assert!(
            error.contains(&definition.hash) && error.contains(&driver_hash),
            "{error}"
        );
        Ok(())
    }

    #[test]
    fn replay_rejects_definition_identity_disagreeing_with_driver() -> Result<()> {
        let store = Store::memory()?;
        let definition = definition(&store)?;
        let source_hash = store.put("source_bundle", b"source")?;
        let driver_hash = store.put("item-preimage", b"different driver entry")?;
        store.record_definition_event(&serde_json::json!({"type":"defined","def":definition,"source_hash":source_hash,"deps":{},"identity":{
            "behavior_hash":driver_hash,"wasm_hash":"wasm","toolchain_hash":"toolchain","item_hashes_ref":source_hash
        }}))?;
        let error = store.rebuild_views().unwrap_err().to_string();
        assert!(
            error.contains(&definition.hash) && error.contains(&driver_hash),
            "{error}"
        );
        Ok(())
    }
}
