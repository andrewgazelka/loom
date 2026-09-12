use super::*;

impl Store {
    pub fn define(
        &self,
        def: &Def,
        name: Option<&str>,
        source: &str,
        deps: &BTreeMap<String, String>,
    ) -> Result<i64> {
        self.recording.barrier(false)?;
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let identity = loom_proto::definition_identity(
            def.lang,
            source,
            deps,
            def.allowed_effects.as_deref(),
        )?;
        ensure!(
            blake3::hash(&identity).to_hex().as_str() == def.hash,
            "definition hash does not match canonical identity"
        );
        put(&tx, "def", &identity)?;
        let mut def = def.clone();
        if let Some(labels) = def.allowed_effects.as_mut() {
            labels.sort();
            labels.dedup();
        }
        def.observed_effects.clear();
        let source_hash = put(&tx, "source_bundle", source.as_bytes())?;
        let event = serde_json::json!({"type":"defined","def":def,"name":name,"source_hash":source_hash,"deps":deps});
        let seq = append(&tx, "system", &event, 0)?;
        tx.execute("INSERT INTO defs(hash,lang,name_hint,type_sig,component_hash,source_hash,allowed_effects) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(hash) DO UPDATE SET component_hash=coalesce(excluded.component_hash,defs.component_hash)",params![def.hash,def.lang.as_str(),name,serde_json::to_string(&def.sig)?,def.component_hash,source_hash,def.allowed_effects.as_ref().map(serde_json::to_string).transpose()?])?;
        for hash in deps.values() {
            tx.execute(
                "INSERT OR IGNORE INTO def_deps VALUES (?,?)",
                params![def.hash, hash],
            )?;
        }
        if let Some(name) = name {
            tx.execute(
                "INSERT INTO names VALUES (?,?,?)",
                params![name, def.hash, seq],
            )?;
        }
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
            return self.definition(hash);
        }
        if let Some(def) = self.definition(name)? {
            return Ok(Some(def));
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
        let bytes:Vec<u8>=c.query_row("SELECT bytes FROM events WHERE actor='system' AND json_extract(bytes,'$.type')='defined' AND json_extract(bytes,'$.def.hash')=? ORDER BY seq DESC LIMIT 1",[hash],|r|r.get(0))?;
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
