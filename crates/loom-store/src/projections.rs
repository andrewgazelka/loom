use super::*;

impl Store {
    /// Reconstruct durable projections from the append-only log. Snapshots are disposable.
    pub fn rebuild_views(&self) -> Result<()> {
        let _publication = self.recording.publication()?;
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let recorded: Vec<Event> = {
            let mut q =
                tx.prepare("SELECT seq,actor,bytes,handler_seq,ts FROM events ORDER BY seq")?;
            let mut rows = q.query([])?;
            let mut result = Vec::new();
            while let Some(r) = rows.next()? {
                result.push(Event {
                    seq: r.get(0)?,
                    actor: r.get(1)?,
                    event: serde_json::from_slice(&r.get::<_, Vec<u8>>(2)?)?,
                    handler_seq: r.get(3)?,
                    ts: r.get(4)?,
                });
            }
            result
        };
        let signature_replacements = migration::replacements(&recorded)?;
        tx.execute_batch("DELETE FROM def_effects; DELETE FROM message_keys; DELETE FROM inbox; DELETE FROM sessions; DELETE FROM snapshots; DELETE FROM names; DELETE FROM def_deps; DELETE FROM defs; DELETE FROM actors; DELETE FROM effect_results;")?;
        for record in recorded {
            let e = &record.event;
            if record.actor != "system" {
                if let Some(initial) = e.get("__loom_init") {
                    let hash = put_value(&tx, "state", initial)?;
                    tx.execute("INSERT OR REPLACE INTO snapshots SELECT id,behavior_hash,?,? FROM actors WHERE id=?", params![record.seq,hash,record.actor])?;
                }
                tx.execute(
                    "UPDATE actors SET last_seq=? WHERE id=?",
                    params![record.seq, record.actor],
                )?;
                continue;
            }
            match e.get("type").and_then(Value::as_str) {
                Some("machine_root_pinned") => {
                    let actor = e["actor"].as_str().context("machine pin missing actor")?;
                    let hash = e["state_hash"]
                        .as_str()
                        .context("machine pin missing state")?;
                    tx.execute("INSERT OR REPLACE INTO snapshots SELECT id,behavior_hash,?,? FROM actors WHERE id=?", params![record.seq,hash,actor])?;
                }
                Some("effect_invoked") => {
                    record_observed_effect(&tx, e)?;
                }
                Some("dag_cbor_migrated") => {
                    tx.execute_batch("UPDATE defs SET component_hash=NULL; UPDATE actors SET component_hash=NULL;")?;
                }
                Some("message_enqueued") => {
                    if let Some(key) = e["key"].as_str() {
                        let hash = put_value(&tx, "message", &e["msg"])?;
                        tx.execute(
                            "INSERT INTO message_keys VALUES (?,?,?,?)",
                            params![
                                key,
                                e["actor"].as_str().context("missing actor")?,
                                record.seq,
                                hash
                            ],
                        )?;
                    }
                    tx.execute(
                        "INSERT INTO inbox VALUES (?,?,?)",
                        params![
                            e["actor"].as_str().context("missing actor")?,
                            record.seq,
                            serde_json::to_string(&e["msg"])?
                        ],
                    )?;
                }
                Some("message_completed") => {
                    tx.execute(
                        "DELETE FROM inbox WHERE actor=? AND handler_seq=?",
                        params![
                            e["actor"].as_str().context("missing actor")?,
                            e["handler_seq"]
                                .as_i64()
                                .context("missing handler sequence")?
                        ],
                    )?;
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
                Some("actor_created") => {
                    let a: Actor = serde_json::from_value(e["actor"].clone())?;
                    tx.execute(
                        "INSERT INTO actors VALUES (?,?,?,?,?,?,?)",
                        params![
                            a.id,
                            a.behavior_hash,
                            a.lang.as_str(),
                            a.component_hash,
                            record.seq,
                            record.seq,
                            a.parent
                        ],
                    )?;
                }
                Some("actor_updated") => {
                    let a: Actor = serde_json::from_value(e["actor"].clone())?;
                    tx.execute("UPDATE actors SET behavior_hash=?,lang=?,component_hash=?,last_seq=? WHERE id=?",params![a.behavior_hash,a.lang.as_str(),a.component_hash,record.seq,a.id])?;
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
                Some("session_created") => {
                    tx.execute(
                        "INSERT INTO sessions VALUES (?,?,?)",
                        params![
                            e["id"].as_str().context("missing id")?,
                            e["actor"].as_str().context("missing actor")?,
                            e["owner"].as_str().context("missing owner")?
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
