//! One publication invariant for live writes and durable-event projection.
use super::*;

pub(super) fn write(
    connection: &Connection,
    candidate: &Def,
    name: Option<&str>,
    source: &str,
    deps: &BTreeMap<String, String>,
    identity: Option<&loom_proto::BuildIdentity>,
) -> Result<i64> {
    ensure!(
        candidate.hash.len() == 64 && candidate.hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid definition hash {}",
        candidate.hash
    );
    let mut def = candidate.clone();
    if let Some(labels) = def.allowed_effects.as_mut() {
        labels.sort();
        labels.dedup();
    }
    def.observed_effects.clear();
    let source_hash = put(connection, "source_bundle", source.as_bytes())?;
    validate(connection, &def)?;
    let event = serde_json::json!({
        "type": "defined", "def": def, "name": name,
        "source_hash": source_hash, "deps": deps, "identity": identity,
    });
    let seq = record_definition_event(connection, &event)?;
    project(connection, &def, name, &source_hash, deps, identity, seq)?;
    Ok(seq)
}

pub(super) fn validate(connection: &Connection, candidate: &Def) -> Result<()> {
    let Some(existing) = executable_definition(connection, &candidate.hash)? else {
        return Ok(());
    };
    ensure!(
        existing.component_hash == candidate.component_hash,
        "definition {} is immutable: component hash {} conflicts with {}",
        candidate.hash,
        existing.component_hash.as_deref().unwrap_or("<none>"),
        candidate.component_hash.as_deref().unwrap_or("<none>")
    );
    let previous_schema = serde_json::to_vec(&existing.sig)?;
    let candidate_schema = serde_json::to_vec(&candidate.sig)?;
    ensure!(
        previous_schema == candidate_schema,
        "definition {} is immutable: schema hash {} conflicts with {}",
        candidate.hash,
        blake3::hash(&previous_schema),
        blake3::hash(&candidate_schema)
    );
    ensure!(
        existing.lang == candidate.lang && existing.allowed_effects == candidate.allowed_effects,
        "definition {} is immutable: language or execution policy differs",
        candidate.hash
    );
    Ok(())
}

pub(super) fn project(
    connection: &Connection,
    definition: &Def,
    name: Option<&str>,
    source_hash: &str,
    deps: &BTreeMap<String, String>,
    identity: Option<&loom_proto::BuildIdentity>,
    seq: i64,
) -> Result<()> {
    if let Some(identity) = identity {
        ensure!(
            definition.hash == identity.behavior_hash,
            "definition hash {} differs from driver export root {}",
            definition.hash,
            identity.behavior_hash
        );
        let bytes: Vec<u8> = connection.query_row(
            "SELECT bytes FROM cas WHERE hash=?",
            [&identity.item_hashes_ref],
            |row| row.get(0),
        )?;
        let document: Value = serde_json::from_slice(&bytes)?;
        let exports: BTreeMap<String, String> = serde_json::from_value(document["exports"].clone())
            .context("driver document has no exports; rebuild with the current driver")?;
        ensure!(!exports.is_empty(), "driver export root has no exports");
        let root = blake3::hash(&loom_proto::export_identity_preimage(&exports))
            .to_hex()
            .to_string();
        ensure!(
            root == definition.hash,
            "definition hash {} differs from computed driver export root {root}",
            definition.hash
        );
        ensure!(
            connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM cas WHERE hash=?)",
                [&definition.hash],
                |row| row.get::<_, bool>(0)
            )?,
            "driver export root preimage {} not found in CAS",
            definition.hash
        );
    }
    validate(connection, definition)?;
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM defs WHERE hash=?)",
        [&definition.hash],
        |row| row.get(0),
    )?;
    if exists {
        let recorded: Vec<u8> = connection.query_row("SELECT bytes FROM definition_events WHERE json_extract(bytes,'$.type')='defined' AND json_extract(bytes,'$.def.hash')=? ORDER BY seq LIMIT 1", [&definition.hash], |row| row.get(0))?;
        let event: Value = serde_json::from_slice(&recorded)?;
        let previous: BTreeMap<String, String> = serde_json::from_value(event["deps"].clone())?;
        ensure!(
            previous == *deps,
            "definition {} is immutable: dependency pins hash {} conflicts with {}",
            definition.hash,
            blake3::hash(&serde_json::to_vec(&previous)?),
            blake3::hash(&serde_json::to_vec(deps)?)
        );
    }
    connection.execute(
        "INSERT INTO defs(hash,lang,name_hint,type_sig,component_hash,source_hash,allowed_effects) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(hash) DO UPDATE SET source_hash=excluded.source_hash",
        params![definition.hash,definition.lang.as_str(),name,serde_json::to_string(&definition.sig)?,definition.component_hash,source_hash,definition.allowed_effects.as_ref().map(serde_json::to_string).transpose()?],
    )?;
    if let Some(identity) = identity {
        connection.execute("UPDATE defs SET behavior_hash=?,wasm_hash=?,toolchain_hash=?,item_hashes_ref=? WHERE hash=?",params![identity.behavior_hash,identity.wasm_hash,identity.toolchain_hash,identity.item_hashes_ref,definition.hash])?;
    }
    connection.execute(
        "INSERT OR IGNORE INTO source_revisions VALUES (?,?,?)",
        params![definition.hash, source_hash, seq],
    )?;
    for hash in deps.values() {
        connection.execute(
            "INSERT OR IGNORE INTO def_deps VALUES (?,?)",
            params![definition.hash, hash],
        )?;
    }
    if let Some(name) = name {
        let previous: Option<String> = connection
            .query_row(
                "SELECT hash FROM names WHERE name=? ORDER BY since_seq DESC LIMIT 1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        if previous.as_deref() != Some(&definition.hash) {
            connection.execute(
                "INSERT INTO names VALUES (?,?,?)",
                params![name, definition.hash, seq],
            )?;
        }
    }
    Ok(())
}
