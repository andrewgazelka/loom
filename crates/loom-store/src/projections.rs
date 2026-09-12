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
        tx.execute_batch("DELETE FROM def_effects; DELETE FROM names; DELETE FROM def_deps; DELETE FROM source_revisions; DELETE FROM defs; DELETE FROM effect_results;")?;
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
                    let identity = if e["identity"].is_null() {
                        None
                    } else {
                        Some(serde_json::from_value::<loom_proto::BuildIdentity>(
                            e["identity"].clone(),
                        )?)
                    };
                    super::publication::project(
                        &tx,
                        &def,
                        e["name"].as_str(),
                        e["source_hash"].as_str().context("missing source hash")?,
                        &serde_json::from_value(e["deps"].clone())?,
                        identity.as_ref(),
                        record.seq,
                    )?;
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
