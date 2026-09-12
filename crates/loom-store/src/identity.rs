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
        let mut query = connection.prepare("SELECT name,hash FROM names n WHERE since_seq=(SELECT MAX(since_seq) FROM names WHERE name=n.name) ORDER BY name")?;
        let mut rows = query.query([])?;
        let mut names = BTreeMap::new();
        while let Some(row) = rows.next()? {
            names.insert(row.get(0)?, row.get(1)?);
        }
        Ok(names)
    }
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
        let definition = Def {
            hash: blake3::hash(&loom_proto::definition_identity(
                loom_proto::Lang::Rust,
                source,
                &deps,
                None,
            )?)
            .to_hex()
            .to_string(),
            lang: loom_proto::Lang::Rust,
            component_hash: None,
            sig: Default::default(),
            allowed_effects: None,
            observed_effects: Vec::new(),
        };
        let identity = loom_proto::BuildIdentity {
            behavior_hash: store.put("blob", b"behavior")?,
            wasm_hash: store.put("blob", b"wasm")?,
            toolchain_hash: store.put("blob", b"toolchain")?,
            item_hashes_ref: store.put("blob", b"{}")?,
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
