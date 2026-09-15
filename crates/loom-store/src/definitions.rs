/// An entry hash may occur in several revisions; the earliest publication owns
/// its stable address, so later unrelated entries cannot change its execution.
pub struct EntryReference {
    pub definition_hash: String,
    pub name: String,
}

use super::*;

impl Store {
    pub fn resolve_entry(&self, hash: &str) -> Result<Option<EntryReference>> {
        Ok(self.lock()?.query_row(
            "SELECT d.hash,e.key FROM defs d JOIN cas c ON c.hash=d.item_hashes_ref JOIN json_each(CAST(c.bytes AS TEXT),'$.entry') e WHERE e.value=? ORDER BY (SELECT MIN(seq) FROM definition_events WHERE json_extract(bytes,'$.def.hash')=d.hash),d.hash,e.key LIMIT 1",
            [hash.strip_prefix('#').unwrap_or(hash)],
            |row| Ok(EntryReference { definition_hash: row.get(0)?, name: row.get(1)? }),
        ).optional()?)
    }

    pub fn define(
        &self,
        def: &Def,
        name: Option<&str>,
        source: &str,
        deps: &BTreeMap<String, String>,
    ) -> Result<i64> {
        self.define_with_identity(def, name, source, deps, None)
    }
    pub fn define_with_identity(
        &self,
        def: &Def,
        name: Option<&str>,
        source: &str,
        deps: &BTreeMap<String, String>,
        identity: Option<&loom_proto::BuildIdentity>,
    ) -> Result<i64> {
        self.recording.barrier(false)?;
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let seq = publication::write(&tx, def, name, source, deps, identity)?;
        tx.commit()?;
        Ok(seq)
    }
    /// Execution metadata is published synchronously. Reading it does not drain
    /// effect recordings; observed_effects is intentionally excluded.
    pub fn executable_definition(&self, hash: &str) -> Result<Option<Def>> {
        executable_definition(&*self.lock()?, hash)
    }
    pub fn definition(&self, hash: &str) -> Result<Option<Def>> {
        self.recording.barrier(false)?;
        definition(&*self.lock()?, hash)
    }
    pub fn resolve(&self, name: &str) -> Result<Option<Def>> {
        if let Some(hash) = name.strip_prefix('#') {
            return self.resolve(hash);
        }
        if let Some(def) = self.definition(name)? {
            return Ok(Some(def));
        }
        if let Some(entry) = self.resolve_entry(name)? {
            return self.definition(&entry.definition_hash);
        }
        let connection = self.lock()?;
        let hash: Option<String> = connection
            .query_row(
                "SELECT hash FROM names WHERE name=? ORDER BY since_seq DESC LIMIT 1",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        hash.map(|h| definition(&connection, &h))
            .transpose()
            .map(Option::flatten)
    }
    pub fn definition_name(&self, hash: &str) -> Result<Option<String>> {
        Ok(self
            .lock()?
            .query_row("SELECT name_hint FROM defs WHERE hash=?", [hash], |r| {
                r.get(0)
            })
            .optional()?
            .flatten())
    }
    pub fn definitions(&self) -> Result<Vec<Def>> {
        self.recording.barrier(false)?;
        let c = self.lock()?;
        let mut q = c.prepare("SELECT hash FROM defs ORDER BY hash")?;
        let hashes: Vec<String> = q
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        hashes
            .iter()
            .map(|h| definition(&c, h)?.context("definition disappeared"))
            .collect()
    }
    /// Load the immutable executable payload, independent of display-source
    /// revisions. A changed engine ABI requires explicit re-admission; old
    /// actors must never silently execute a new host contract under an old hash.
    pub fn javascript_source(&self, hash: &str, backend_abi: &str) -> Result<String> {
        let definition = self
            .executable_definition(hash)?
            .context("JavaScript definition missing")?;
        ensure!(
            definition.lang == loom_proto::Lang::JavaScript,
            "definition {hash} is not JavaScript"
        );
        let artifact = definition
            .component_hash
            .as_ref()
            .context("JavaScript definition has no executable source")?;
        let bytes = self
            .get(artifact)?
            .context("JavaScript executable source missing")?;
        ensure!(
            blake3::hash(&bytes).to_hex().as_str() == artifact.as_str(),
            "JavaScript executable source hash mismatch for {hash}"
        );
        let source =
            String::from_utf8(bytes).context("JavaScript executable source is not UTF-8")?;
        let deps = self.definition_deps(hash)?;
        ensure!(
            deps.is_empty(),
            "JavaScript definitions do not support imports or dependencies"
        );
        let identity = loom_proto::javascript_definition_identity(
            &source,
            &deps,
            definition.allowed_effects.as_deref(),
            backend_abi,
        )?;
        ensure!(
            blake3::hash(&identity).to_hex().as_str() == hash,
            "JavaScript definition identity mismatch for {hash}: source, policy, dependencies, or engine ABI changed"
        );
        Ok(source)
    }

    pub fn source(&self, hash: &str) -> Result<Option<String>> {
        Ok(self.lock()?.query_row("SELECT CAST(c.bytes AS TEXT) FROM defs d JOIN cas c ON c.hash=d.source_hash WHERE d.hash=?",[hash],|r|r.get(0)).optional()?)
    }
    pub fn dependencies(&self, hash: &str) -> Result<Vec<String>> {
        let connection = self.lock()?;
        let mut q = connection
            .prepare("SELECT dep_hash FROM def_deps WHERE def_hash=? ORDER BY dep_hash")?;
        Ok(q.query_map([hash], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn dependents(&self, hash: &str) -> Result<Vec<String>> {
        let c = self.lock()?;
        let mut q =
            c.prepare("SELECT def_hash FROM def_deps WHERE dep_hash=? ORDER BY def_hash")?;
        Ok(q.query_map([hash], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn definition_deps(&self, hash: &str) -> Result<BTreeMap<String, String>> {
        let c = self.lock()?;
        let bytes:Vec<u8>=c.query_row("SELECT bytes FROM definition_events WHERE json_extract(bytes,'$.type')='defined' AND json_extract(bytes,'$.def.hash')=? ORDER BY seq LIMIT 1",[hash],|r|r.get(0))?;
        let event: Value = serde_json::from_slice(&bytes)?;
        Ok(serde_json::from_value(event["deps"].clone())?)
    }
    pub fn name_history(&self, name: &str) -> Result<Vec<loom_proto::NameRevision>> {
        let c = self.lock()?;
        let mut q =
            c.prepare("SELECT name,hash,since_seq FROM names WHERE name=? ORDER BY since_seq")?;
        Ok(q.query_map([name], |r| {
            Ok(loom_proto::NameRevision {
                name: r.get(0)?,
                hash: r.get(1)?,
                since_seq: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
    }
}
