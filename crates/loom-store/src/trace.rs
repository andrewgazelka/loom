use crate::{Store, append};
use anyhow::{Context, Result, ensure};
use loom_proto::{CallTrace, TraceBlob, TraceBlobKind, TraceBundle, TraceOutcome};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};

impl Store {
    /// Atomically publish a completed call or an actor recovery checkpoint.
    /// Blob bytes are produced by the trusted host codec; this boundary verifies
    /// their identity without decoding and re-encoding each effect result.
    pub fn persist_call_trace(&self, bundle: &TraceBundle) -> Result<String> {
        self.recording.trace(bundle)
    }
    pub fn load_call_trace(&self, scope: &str) -> Result<Option<TraceBundle>> {
        self.recording.barrier(false)?;
        let connection = self.lock()?;
        let hash: Option<String> = connection
            .query_row(
                "SELECT trace_hash FROM call_traces WHERE scope=?",
                [scope],
                |row| row.get(0),
            )
            .optional()?;
        let Some(hash) = hash else {
            return Ok(None);
        };
        let bytes = read_trace_bytes(&connection, &hash)?;
        let trace: CallTrace = loom_proto::decode_call_trace(&bytes).map_err(anyhow::Error::msg)?;
        ensure!(
            trace.entries.len() <= loom_proto::TRACE_MAX_ENTRIES,
            "trace entry limit exceeded"
        );
        let mut needed = BTreeMap::new();
        if let Some(hash) = &trace.args_hash {
            needed.insert(hash.clone(), TraceBlobKind::Arguments);
        }
        for entry in &trace.entries {
            needed.insert(entry.descriptor_hash.clone(), TraceBlobKind::Descriptor);
            if let TraceOutcome::Success { result_hash } = &entry.outcome {
                needed.insert(result_hash.clone(), TraceBlobKind::Result);
            }
        }
        if let Some(TraceOutcome::Success { result_hash }) = &trace.outcome {
            needed.insert(result_hash.clone(), TraceBlobKind::Result);
        }
        let mut total_bytes = 0usize;
        let blobs = needed
            .into_iter()
            .map(|(hash, kind)| {
                let size: usize = connection.query_row(
                    "SELECT length(bytes) FROM cas WHERE hash=?",
                    [&hash],
                    |row| row.get(0),
                )?;
                total_bytes = total_bytes
                    .checked_add(size)
                    .context("trace byte count overflow")?;
                ensure!(
                    total_bytes <= loom_proto::TRACE_MAX_BLOB_BYTES,
                    "trace blob byte limit exceeded"
                );
                let bytes =
                    connection.query_row("SELECT bytes FROM cas WHERE hash=?", [&hash], |row| {
                        row.get(0)
                    })?;
                Ok(TraceBlob { hash, kind, bytes })
            })
            .collect::<Result<Vec<_>>>()?;
        let event: Vec<u8> = connection.query_row(
            "SELECT e.bytes FROM call_traces t JOIN events e ON e.seq=t.last_seq WHERE t.scope=?",
            [scope],
            |row| row.get(0),
        )?;
        let event: serde_json::Value = serde_json::from_slice(&event)?;
        let observations = serde_json::from_value(
            event
                .get("observations")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
        )?;
        Ok(Some(TraceBundle {
            trace,
            observations,
            blobs,
            memos: Vec::new(),
        }))
    }
}
fn insert_blob(connection: &Connection, hash: &str, kind: &str, bytes: &[u8]) -> Result<()> {
    ensure!(
        blake3::hash(bytes).to_hex().as_str() == hash,
        "trace blob hash mismatch"
    );
    connection.prepare_cached("INSERT OR IGNORE INTO cas(hash,kind,bytes,created_at,codec) VALUES (?,?,?,unixepoch(),113)")?.execute(params![hash,kind,bytes])?;
    connection
        .prepare_cached("INSERT OR IGNORE INTO cas_codecs VALUES (?,113)")?
        .execute([hash])?;
    Ok(())
}
fn require_blob(connection: &Connection, supplied: &BTreeSet<String>, hash: &str) -> Result<()> {
    ensure!(
        supplied.contains(hash)
            || connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM cas WHERE hash=? AND codec=113)",
                [hash],
                |row| row.get::<_, bool>(0)
            )?,
        "trace refers to missing blob {hash}"
    );
    Ok(())
}
pub(super) fn persist(
    connection: &Connection,
    bundle: &TraceBundle,
    hash: &str,
    bytes: &[u8],
) -> Result<()> {
    ensure!(bundle.trace.version == 1, "unsupported call trace version");
    ensure!(
        !bundle.trace.scope.is_empty(),
        "call trace scope must not be empty"
    );
    ensure!(
        bundle.trace.entries.len() <= loom_proto::TRACE_MAX_ENTRIES,
        "trace entry limit exceeded"
    );
    let mut total_bytes = 0usize;
    for blob in &bundle.blobs {
        total_bytes = total_bytes
            .checked_add(blob.bytes.len())
            .context("trace byte count overflow")?;
        ensure!(
            total_bytes <= loom_proto::TRACE_MAX_BLOB_BYTES,
            "trace blob byte limit exceeded"
        );
    }
    let mut keys = BTreeSet::new();
    let mut supplied = BTreeSet::new();
    for blob in &bundle.blobs {
        insert_blob(connection, &blob.hash, blob.kind.as_str(), &blob.bytes)?;
        supplied.insert(blob.hash.clone());
    }
    for entry in &bundle.trace.entries {
        ensure!(keys.insert(entry.key.clone()), "duplicate trace occurrence");
        ensure!(entry.key.occurrence >= 0, "negative trace occurrence");
        require_blob(connection, &supplied, &entry.descriptor_hash)?;
        if let TraceOutcome::Success { result_hash } = &entry.outcome {
            require_blob(connection, &supplied, result_hash)?;
        }
    }
    if let Some(TraceOutcome::Success { result_hash }) = &bundle.trace.outcome {
        require_blob(connection, &supplied, result_hash)?;
    }
    if let Some(hash) = &bundle.trace.args_hash {
        require_blob(connection, &supplied, hash)?;
    }
    let existing: Option<ExistingTrace> = connection
        .query_row(
            "SELECT trace_hash,completed FROM call_traces WHERE scope=?",
            [&bundle.trace.scope],
            |row| {
                Ok(ExistingTrace {
                    hash: row.get(0)?,
                    completed: row.get(1)?,
                })
            },
        )
        .optional()?;
    if let Some(existing) = &existing {
        if existing.hash == hash {
            return Ok(());
        }
        ensure!(
            !existing.completed,
            "completed trace conflicts with recorded call"
        );
        let previous_bytes = read_trace_bytes(connection, &existing.hash)?;
        let previous =
            loom_proto::decode_call_trace(&previous_bytes).map_err(anyhow::Error::msg)?;
        ensure!(
            previous
                .definition_hash
                .as_ref()
                .is_none_or(|identity| Some(identity) == bundle.trace.definition_hash.as_ref())
                && previous
                    .args_hash
                    .as_ref()
                    .is_none_or(|identity| Some(identity) == bundle.trace.args_hash.as_ref()),
            "trace checkpoint identity changed"
        );
        let current: BTreeMap<_, _> = bundle
            .trace
            .entries
            .iter()
            .map(|entry| (&entry.key, entry))
            .collect();
        for old in previous.entries {
            let next = current
                .get(&old.key)
                .context("trace checkpoint removed an occurrence")?;
            ensure!(
                next.descriptor_hash == old.descriptor_hash,
                "trace checkpoint changed descriptor"
            );
            ensure!(
                old.outcome == TraceOutcome::Cancelled || next.outcome == old.outcome,
                "trace checkpoint changed recorded result"
            );
        }
    }
    insert_blob(connection, hash, "trace", bytes)?;
    for memo in &bundle.memos {
        require_blob(connection, &supplied, &memo.result_hash)?;
        let existing: Option<String> = connection.query_row("SELECT result_hash FROM effect_results WHERE desc_hash=? AND scope=? AND occurrence=?", params![memo.descriptor_hash,memo.scope,memo.occurrence], |row| row.get(0)).optional()?;
        ensure!(
            existing.as_ref().is_none_or(|old| old == &memo.result_hash),
            "effect cache result conflict"
        );
        connection.execute(
            "INSERT OR IGNORE INTO effect_results VALUES (?,?,?,?)",
            params![
                memo.descriptor_hash,
                memo.scope,
                memo.occurrence,
                memo.result_hash
            ],
        )?;
    }
    project_observations(connection, &bundle.observations)?;
    let completed = bundle.trace.outcome.is_some();
    let event = serde_json::json!({"type":if completed {"call_completed"} else {"call_checkpoint"}, "scope":bundle.trace.scope,"definition_hash":bundle.trace.definition_hash,"args_hash":bundle.trace.args_hash,"trace_hash":hash,"outcome":bundle.trace.outcome,"memos":bundle.memos,"observations":bundle.observations});
    let seq = append(connection, "system", &event, 0)?;
    connection.execute("INSERT INTO call_traces(scope,trace_hash,completed,last_seq) VALUES (?,?,?,?) ON CONFLICT(scope) DO UPDATE SET trace_hash=excluded.trace_hash,completed=excluded.completed,last_seq=excluded.last_seq", params![bundle.trace.scope,hash,completed,seq])?;
    Ok(())
}
struct ExistingTrace {
    hash: String,
    completed: bool,
}

pub(super) fn rebuild(connection: &Connection) -> Result<()> {
    connection.execute("DELETE FROM call_traces", [])?;
    let events: Vec<ProjectionEvent> = {
        let mut query = connection.prepare("SELECT seq,bytes FROM events WHERE json_extract(bytes,'$.type') IN ('call_completed','call_checkpoint') ORDER BY seq")?;
        query
            .query_map([], |row| {
                Ok(ProjectionEvent {
                    seq: row.get(0)?,
                    bytes: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?
    };
    for event in events {
        let value: serde_json::Value = serde_json::from_slice(&event.bytes)?;
        connection.execute("INSERT INTO call_traces VALUES (?,?,?,?) ON CONFLICT(scope) DO UPDATE SET trace_hash=excluded.trace_hash,completed=excluded.completed,last_seq=excluded.last_seq", params![value["scope"].as_str().context("missing trace scope")?,value["trace_hash"].as_str().context("missing trace hash")?,value["type"] == "call_completed",event.seq])?;
        let observations: Vec<loom_proto::TraceObservation> = serde_json::from_value(
            value
                .get("observations")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
        )?;
        project_observations(connection, &observations)?;
        let memos: Vec<loom_proto::TraceMemo> = serde_json::from_value(
            value
                .get("memos")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
        )?;
        for memo in memos {
            connection.execute(
                "INSERT OR IGNORE INTO effect_results VALUES (?,?,?,?)",
                params![
                    memo.descriptor_hash,
                    memo.scope,
                    memo.occurrence,
                    memo.result_hash
                ],
            )?;
        }
    }
    Ok(())
}
struct ProjectionEvent {
    seq: i64,
    bytes: Vec<u8>,
}

/// Convert the former per-effect recovery projection into partial call traces.
/// Explicit host memo keys that are not descriptor CAS identities remain memos.
/// Original events and blobs are retained as historical evidence.
pub(super) fn migrate_legacy(connection: &Connection) -> Result<()> {
    struct LegacyEffect {
        descriptor_hash: String,
        scope: String,
        occurrence: i64,
        result_hash: String,
    }
    let effects: Vec<LegacyEffect> = {
        let mut query = connection.prepare("SELECT e.desc_hash,e.scope,e.occurrence,e.result_hash FROM effect_results e JOIN cas d ON d.hash=e.desc_hash WHERE e.scope!='global' AND d.kind='desc' ORDER BY e.scope,e.occurrence,e.desc_hash")?;
        query
            .query_map([], |row| {
                Ok(LegacyEffect {
                    descriptor_hash: row.get(0)?,
                    scope: row.get(1)?,
                    occurrence: row.get(2)?,
                    result_hash: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut calls: BTreeMap<String, Vec<loom_proto::TraceEntry>> = BTreeMap::new();
    for effect in &effects {
        let root = effect
            .scope
            .split('/')
            .next()
            .context("missing legacy call scope")?;
        calls
            .entry(root.to_owned())
            .or_default()
            .push(loom_proto::TraceEntry {
                key: loom_proto::TraceKey {
                    scope: effect.scope.clone(),
                    occurrence: effect.occurrence,
                },
                descriptor_hash: effect.descriptor_hash.clone(),
                outcome: TraceOutcome::Success {
                    result_hash: effect.result_hash.clone(),
                },
            });
    }
    // Legacy failures had no effect_results row, but their terminal audit event
    // still carries the descriptor and occurrence required for deterministic replay.
    let failures: Vec<Vec<u8>> = {
        let mut query = connection.prepare("SELECT e.bytes FROM events e JOIN cas d ON d.hash=json_extract(e.bytes,'$.desc_hash') WHERE json_extract(e.bytes,'$.type')='effect_completed' AND json_type(e.bytes,'$.error')='text' AND json_type(e.bytes,'$.scope')='text' AND json_type(e.bytes,'$.occurrence')='integer' AND d.kind='desc' ORDER BY e.seq")?;
        query
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for bytes in failures {
        let event: serde_json::Value = serde_json::from_slice(&bytes)?;
        let scope = event["scope"]
            .as_str()
            .context("missing legacy failure scope")?;
        if scope == "global" {
            continue;
        }
        let root = scope
            .split('/')
            .next()
            .context("missing legacy failure root")?;
        let key = loom_proto::TraceKey {
            scope: scope.to_owned(),
            occurrence: event["occurrence"]
                .as_i64()
                .context("invalid legacy failure occurrence")?,
        };
        let entries = calls.entry(root.to_owned()).or_default();
        if entries.iter().any(|entry| entry.key == key) {
            continue;
        }
        entries.push(loom_proto::TraceEntry {
            key,
            descriptor_hash: event["desc_hash"]
                .as_str()
                .context("missing legacy failure descriptor")?
                .to_owned(),
            outcome: TraceOutcome::Error {
                message: event["error"]
                    .as_str()
                    .context("missing legacy failure message")?
                    .to_owned(),
            },
        });
    }
    for (scope, mut entries) in calls {
        entries.sort_by(|left, right| left.key.cmp(&right.key));
        let existing: Option<String> = connection
            .query_row(
                "SELECT trace_hash FROM call_traces WHERE scope=?",
                [&scope],
                |row| row.get(0),
            )
            .optional()?;
        let trace = if let Some(hash) = existing {
            let mut previous = loom_proto::decode_call_trace(&read_trace_bytes(connection, &hash)?)
                .map_err(anyhow::Error::msg)?;
            if previous.outcome.is_some() {
                let recorded: BTreeMap<_, _> = previous
                    .entries
                    .iter()
                    .map(|entry| (&entry.key, entry))
                    .collect();
                for entry in &entries {
                    ensure!(
                        recorded.get(&entry.key).is_some_and(|old| **old == *entry),
                        "legacy occurrence conflicts with completed trace"
                    );
                }
                continue;
            }
            let mut merged: BTreeMap<_, _> = previous
                .entries
                .into_iter()
                .map(|entry| (entry.key.clone(), entry))
                .collect();
            for entry in entries {
                if let Some(old) = merged.get_mut(&entry.key) {
                    ensure!(
                        old.descriptor_hash == entry.descriptor_hash
                            && (*old == entry || old.outcome == TraceOutcome::Cancelled),
                        "legacy occurrence conflicts with trace checkpoint"
                    );
                    if old.outcome == TraceOutcome::Cancelled {
                        *old = entry;
                    }
                } else {
                    merged.insert(entry.key.clone(), entry);
                }
            }
            previous.entries = merged.into_values().collect();
            previous
        } else {
            CallTrace {
                version: 1,
                definition_hash: None,
                args_hash: None,
                scope,
                entries,
                outcome: None,
            }
        };
        let bytes = loom_proto::encode_call_trace(&trace).map_err(anyhow::Error::msg)?;
        let verified = loom_proto::decode_call_trace(&bytes).map_err(anyhow::Error::msg)?;
        ensure!(
            verified == trace,
            "legacy trace migration changed effect records"
        );
        let hash = blake3::hash(&bytes).to_hex().to_string();
        persist(
            connection,
            &TraceBundle {
                trace,
                blobs: Vec::new(),
                memos: Vec::new(),
                observations: Vec::new(),
            },
            &hash,
            &bytes,
        )?;
    }
    for effect in effects {
        connection.execute(
            "DELETE FROM effect_results WHERE desc_hash=? AND scope=? AND occurrence=?",
            params![effect.descriptor_hash, effect.scope, effect.occurrence],
        )?;
    }
    Ok(())
}

#[derive(Debug, serde::Serialize)]
pub struct TraceEffect {
    pub key: loom_proto::TraceKey,
    pub descriptor_hash: String,
    pub op: String,
    pub outcome: TraceOutcome,
}
#[derive(Debug, serde::Serialize)]
pub struct TraceEffectsPage {
    pub trace_hash: String,
    pub scope: String,
    pub definition_hash: Option<String>,
    pub entries: Vec<TraceEffect>,
    pub next_offset: Option<usize>,
}
impl Store {
    /// Page one immutable trace without loading any effect result blobs.
    pub fn trace_effects(
        &self,
        hash: &str,
        offset: usize,
        limit: usize,
    ) -> Result<TraceEffectsPage> {
        ensure!(
            (1..=256).contains(&limit),
            "trace page limit must be between 1 and 256"
        );
        self.recording.barrier(false)?;
        let connection = self.lock()?;
        let bytes = read_trace_bytes(&connection, hash)?;
        let trace = loom_proto::decode_call_trace(&bytes).map_err(anyhow::Error::msg)?;
        ensure!(
            offset <= trace.entries.len(),
            "trace page offset exceeds entry count"
        );
        let end = offset.saturating_add(limit).min(trace.entries.len());
        let mut entries = Vec::with_capacity(end - offset);
        for entry in &trace.entries[offset..end] {
            let descriptor: Vec<u8> = connection.query_row(
                "SELECT bytes FROM cas WHERE hash=?",
                [&entry.descriptor_hash],
                |row| row.get(0),
            )?;
            let descriptor: serde_json::Value =
                loom_proto::decode(&descriptor).map_err(anyhow::Error::msg)?;
            entries.push(TraceEffect {
                key: entry.key.clone(),
                descriptor_hash: entry.descriptor_hash.clone(),
                op: descriptor["op"]
                    .as_str()
                    .context("trace descriptor has no operation")?
                    .to_owned(),
                outcome: entry.outcome.clone(),
            });
        }
        Ok(TraceEffectsPage {
            trace_hash: hash.to_owned(),
            scope: trace.scope,
            definition_hash: trace.definition_hash,
            entries,
            next_offset: (end < trace.entries.len()).then_some(end),
        })
    }
}

fn read_trace_bytes(connection: &Connection, hash: &str) -> Result<Vec<u8>> {
    let size: usize = connection
        .query_row(
            "SELECT length(bytes) FROM cas WHERE hash=? AND kind='trace' AND codec=113",
            [hash],
            |row| row.get(0),
        )
        .optional()?
        .context("unknown call trace")?;
    ensure!(
        size <= loom_proto::TRACE_MAX_METADATA_BYTES,
        "trace encoded byte limit exceeded"
    );
    Ok(
        connection.query_row("SELECT bytes FROM cas WHERE hash=?", [hash], |row| {
            row.get(0)
        })?,
    )
}

fn project_observations(
    connection: &Connection,
    observations: &[loom_proto::TraceObservation],
) -> Result<()> {
    ensure!(
        observations.len() <= loom_proto::TRACE_MAX_ENTRIES,
        "trace observation limit exceeded"
    );
    let mut insert = connection.prepare_cached("INSERT OR IGNORE INTO def_effects VALUES (?,?)")?;
    for observation in observations {
        ensure!(
            !observation.op.is_empty() && observation.op.len() <= loom_proto::TRACE_MAX_SCOPE_BYTES,
            "invalid observed operation"
        );
        insert.execute(params![observation.definition_hash, observation.op])?;
    }
    Ok(())
}
