use super::*;

impl Store {
    /// Reconstruct definition and effect projections from their durable records.
    pub fn rebuild_views(&self) -> Result<()> {
        let _publication = self.recording.publication()?;
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let recorded: Vec<Event> = {
            let mut q = tx.prepare("SELECT seq,bytes,ts FROM definition_events ORDER BY seq")?;
            let mut rows = q.query([])?;
            let mut result = Vec::new();
            while let Some(r) = rows.next()? {
                result.push(Event {
                    seq: r.get(0)?,
                    event: serde_json::from_slice(&r.get::<_, Vec<u8>>(1)?)?,
                    ts: r.get(2)?,
                });
            }
            result
        };
        let signature_replacements = migration::replacements(&recorded)?;
        tx.execute_batch("DELETE FROM def_effects; DELETE FROM names; DELETE FROM def_deps; DELETE FROM defs; DELETE FROM effect_results;")?;
        for record in recorded {
            let e = &record.event;
            match e.get("type").and_then(Value::as_str) {
                Some("effect_invoked") => {
                    record_observed_effect(&tx, e)?;
                }
                Some("dag_cbor_migrated") => {
                    tx.execute_batch("UPDATE defs SET component_hash=NULL;")?;
                }
                Some("defined") => {
                    let mut definition = e["def"].clone();
                    let hash = definition["hash"]
                        .as_str()
                        .context("definition missing hash")?;
                    if let Some(sig) = signature_replacements.get(hash) {
                        definition["sig"] = sig.clone();
                    }
                    let def: Def = serde_json::from_value(definition)?;
                    tx.execute("INSERT INTO defs(hash,lang,name_hint,type_sig,component_hash,source_hash,allowed_effects) VALUES (?,?,?,?,?,?,?) ON CONFLICT(hash) DO UPDATE SET component_hash=coalesce(excluded.component_hash,defs.component_hash)",params![def.hash,def.lang.as_str(),e["name"].as_str(),serde_json::to_string(&def.sig)?,def.component_hash,e["source_hash"].as_str().context("missing source hash")?,def.allowed_effects.as_ref().map(serde_json::to_string).transpose()?])?;
                    if !e["identity"].is_null() {
                        let identity: loom_proto::BuildIdentity =
                            serde_json::from_value(e["identity"].clone())?;
                        tx.execute("UPDATE defs SET behavior_hash=?,wasm_hash=?,toolchain_hash=?,item_hashes_ref=? WHERE hash=?",params![identity.behavior_hash,identity.wasm_hash,identity.toolchain_hash,identity.item_hashes_ref,def.hash])?;
                    }
                    let deps: BTreeMap<String, String> = serde_json::from_value(e["deps"].clone())?;
                    for hash in deps.values() {
                        tx.execute(
                            "INSERT OR IGNORE INTO def_deps VALUES (?,?)",
                            params![def.hash, hash],
                        )?;
                    }
                    if let Some(name) = e["name"].as_str() {
                        tx.execute(
                            "INSERT INTO names VALUES (?,?,?)",
                            params![name, def.hash, record.seq],
                        )?;
                    }
                }
                Some("effect_recorded") => {
                    tx.execute(
                        "INSERT OR IGNORE INTO effect_results VALUES (?,?,?,?)",
                        params![
                            e["desc_hash"].as_str().context("missing desc")?,
                            e["scope"].as_str().context("missing scope")?,
                            e["occurrence"].as_i64().context("missing occurrence")?,
                            e["result_hash"].as_str().context("missing result")?
                        ],
                    )?;
                }
                _ => {}
            }
        }
        trace::rebuild(&tx)?;
        trace::migrate_legacy(&tx)?;
        tx.commit()?;
        self.recording.effects_changed();
        Ok(())
    }
}
