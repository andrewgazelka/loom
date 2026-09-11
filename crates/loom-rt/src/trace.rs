use super::EffectOutput;
use anyhow::{Context, Result, bail, ensure};
use loom_proto::{
    CallTrace, TraceBlob, TraceBlobKind, TraceBundle, TraceEntry, TraceKey, TraceMemo,
    TraceObservation, TraceOutcome, Value,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

pub(super) struct ExecutionTrace {
    state: Mutex<TraceState>,
    publication: Mutex<()>,
}
struct TraceState {
    scope: String,
    definition_hash: Option<String>,
    args_hash: Option<String>,
    entries: BTreeMap<TraceKey, TraceEntry>,
    blobs: BTreeMap<String, TraceBlob>,
    pending: BTreeSet<TraceKey>,
    consumed: BTreeSet<TraceKey>,
    recovery_required: BTreeSet<TraceKey>,
    replay: bool,
    expected: Option<TraceOutcome>,
    memos: Vec<TraceMemo>,
    observations: BTreeSet<TraceObservation>,
    sealed: bool,
    blob_bytes: usize,
    metadata_bytes: usize,
    limits: TraceLimits,
}
#[derive(Clone, Copy)]
struct TraceLimits {
    entries: usize,
    blob_bytes: usize,
    metadata_bytes: usize,
}
impl Default for TraceLimits {
    fn default() -> Self {
        Self {
            entries: loom_proto::TRACE_MAX_ENTRIES,
            blob_bytes: loom_proto::TRACE_MAX_BLOB_BYTES,
            metadata_bytes: loom_proto::TRACE_MAX_METADATA_BYTES,
        }
    }
}
pub(super) enum StartedEffect {
    Recorded(EffectGuard),
    Replayed(EffectOutput),
}
pub(super) struct EffectGuard {
    trace: Arc<ExecutionTrace>,
    key: TraceKey,
    finished: bool,
}
impl ExecutionTrace {
    pub fn fresh(scope: &str) -> Arc<Self> {
        Arc::new(Self {
            publication: Mutex::new(()),
            state: Mutex::new(TraceState {
                scope: scope.into(),
                definition_hash: None,
                args_hash: None,
                entries: BTreeMap::new(),
                blobs: BTreeMap::new(),
                pending: BTreeSet::new(),
                consumed: BTreeSet::new(),
                recovery_required: BTreeSet::new(),
                replay: false,
                expected: None,
                memos: Vec::new(),
                observations: BTreeSet::new(),
                sealed: false,
                blob_bytes: 0,
                metadata_bytes: loom_proto::TRACE_MAX_ERROR_BYTES + scope.len(),
                limits: TraceLimits::default(),
            }),
        })
    }
    pub fn loaded(bundle: TraceBundle) -> Result<Arc<Self>> {
        Self::loaded_with_limits(bundle, TraceLimits::default())
    }
    fn loaded_with_limits(bundle: TraceBundle, limits: TraceLimits) -> Result<Arc<Self>> {
        ensure!(bundle.trace.version == 1, "unsupported trace version");
        ensure!(
            bundle.trace.entries.len() <= limits.entries,
            "trace entry limit exceeded"
        );
        ensure!(
            bundle.trace.scope.len() <= loom_proto::TRACE_MAX_SCOPE_BYTES,
            "trace scope limit exceeded"
        );
        let trace = Self::fresh(&bundle.trace.scope);
        {
            let mut state = trace.state.lock().unwrap();
            state.limits = limits;
            state.replay = bundle.trace.outcome.is_some();
            if let Some(TraceOutcome::Error { message }) = &bundle.trace.outcome {
                ensure!(
                    message.len() <= loom_proto::TRACE_MAX_ERROR_BYTES,
                    "trace error message limit exceeded"
                );
            }
            state.expected = bundle.trace.outcome.clone();
            state.definition_hash = bundle.trace.definition_hash;
            state.args_hash = bundle.trace.args_hash;
            state.memos = bundle.memos;
            for observation in bundle.observations {
                add_observation(&mut state, observation)?;
            }
            for blob in bundle.blobs {
                ensure!(
                    blake3::hash(&blob.bytes).to_hex().as_str() == blob.hash,
                    "trace blob hash mismatch"
                );
                reserve_blob(&mut state, &blob.hash, blob.bytes.len())?;
                ensure!(
                    state.blobs.insert(blob.hash.clone(), blob).is_none(),
                    "duplicate trace blob"
                );
            }
            for entry in bundle.trace.entries {
                if !matches!(entry.outcome, TraceOutcome::Cancelled) {
                    state.recovery_required.insert(entry.key.clone());
                }
                reserve_entry(&mut state, &entry.key)?;
                if let TraceOutcome::Error { message } = &entry.outcome {
                    reserve_error(&mut state, message)?;
                }
                ensure!(
                    state.entries.insert(entry.key.clone(), entry).is_none(),
                    "duplicate trace key"
                );
            }
        }
        Ok(trace)
    }
    pub fn identity(&self, definition_hash: &str, args: &Value) -> Result<()> {
        let bytes = super::encode(args)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let mut state = self.state.lock().unwrap();
        if state.definition_hash.is_some() || state.replay {
            ensure!(
                state.definition_hash.as_deref() == Some(definition_hash)
                    && state.args_hash.as_deref() == Some(&hash),
                "replay root definition or arguments diverged"
            );
        } else {
            reserve_blob(&mut state, &hash, bytes.len())?;
            state.definition_hash = Some(definition_hash.into());
            state.args_hash = Some(hash.clone());
            state.blobs.insert(
                hash.clone(),
                TraceBlob {
                    hash,
                    kind: TraceBlobKind::Arguments,
                    bytes,
                },
            );
        }
        Ok(())
    }
    pub fn observe(&self, definition_hash: &str, op: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        ensure!(!state.sealed, "execution trace already sealed");
        let size = definition_hash
            .len()
            .saturating_add(op.len())
            .saturating_add(64);
        if size
            > state
                .limits
                .metadata_bytes
                .saturating_sub(state.metadata_bytes)
        {
            ensure!(
                state
                    .observations
                    .iter()
                    .any(|entry| entry.definition_hash == definition_hash && entry.op == op),
                "trace metadata byte limit exceeded"
            );
            return Ok(());
        }
        add_observation(
            &mut state,
            TraceObservation {
                definition_hash: definition_hash.into(),
                op: op.into(),
            },
        )
    }
    pub fn begin(
        self: &Arc<Self>,
        scope: &str,
        occurrence: i64,
        descriptor: &Value,
    ) -> Result<StartedEffect> {
        ensure!(
            scope.len() <= loom_proto::TRACE_MAX_SCOPE_BYTES,
            "trace scope limit exceeded"
        );
        let descriptor_bytes = super::encode(descriptor)?;
        let descriptor_hash = blake3::hash(&descriptor_bytes).to_hex().to_string();
        let key = TraceKey {
            scope: scope.into(),
            occurrence,
        };
        let mut state = self.state.lock().unwrap();
        ensure!(!state.sealed, "execution trace already sealed");
        if let Some(entry) = state.entries.get(&key).cloned() {
            ensure!(
                entry.descriptor_hash == descriptor_hash,
                "replay effect divergence at {scope}:{occurrence}"
            );
            ensure!(
                state.consumed.insert(key.clone()),
                "duplicate effect occurrence at {scope}:{occurrence}"
            );
            return match entry.outcome {
                TraceOutcome::Success { result_hash } => {
                    let bytes = state
                        .blobs
                        .get(&result_hash)
                        .context("trace result blob missing")?
                        .bytes
                        .clone();
                    Ok(StartedEffect::Replayed(EffectOutput { bytes }))
                }
                TraceOutcome::Error { message } => bail!("{message}"),
                TraceOutcome::Cancelled if !state.replay => {
                    state.entries.remove(&key);
                    state.metadata_bytes =
                        state.metadata_bytes.saturating_sub(key.scope.len() + 256);
                    state.consumed.remove(&key);
                    drop(state);
                    self.begin(scope, occurrence, descriptor)
                }
                TraceOutcome::Cancelled => {
                    bail!("replayed cancelled effect at {scope}:{occurrence}")
                }
            };
        }
        ensure!(
            !state.replay,
            "replay missing effect at {scope}:{occurrence}"
        );
        reserve_entry(&mut state, &key)?;
        reserve_blob(&mut state, &descriptor_hash, descriptor_bytes.len())?;
        state.blobs.insert(
            descriptor_hash.clone(),
            TraceBlob {
                hash: descriptor_hash.clone(),
                kind: TraceBlobKind::Descriptor,
                bytes: descriptor_bytes,
            },
        );
        state.entries.insert(
            key.clone(),
            TraceEntry {
                key: key.clone(),
                descriptor_hash,
                outcome: TraceOutcome::Cancelled,
            },
        );
        state.pending.insert(key.clone());
        state.consumed.insert(key.clone());
        Ok(StartedEffect::Recorded(EffectGuard {
            trace: self.clone(),
            key,
            finished: false,
        }))
    }
    /// After all workers in an execution have drained, cancelled occurrences
    /// need no replay result. Successful and failed occurrences must be consumed.
    pub fn finish_scope(&self, scope: &str) {
        let mut state = self.state.lock().unwrap();
        let prefix = format!("{scope}/");
        let cancelled: Vec<_> = state.entries.values()
            .filter(|entry| {
                (entry.key.scope == scope || entry.key.scope.starts_with(&prefix))
                    && matches!(entry.outcome, TraceOutcome::Cancelled)
            })
            .map(|entry| entry.key.clone())
            .collect();
        state.consumed.extend(cancelled);
    }
    pub fn checkpoint(&self, store: &loom_store::Store) -> Result<()> {
        let _publication = self.publication.lock().unwrap();
        store.persist_call_trace(&self.snapshot(None, false)?)?;
        store.flush()
    }
    pub fn snapshot(
        &self,
        outcome: Option<&Result<EffectOutput>>,
        seal: bool,
    ) -> Result<TraceBundle> {
        let mut state = self.state.lock().unwrap();
        if state.replay && seal {
            ensure!(
                state.entries.len() == state.consumed.len(),
                "replay left unconsumed effects"
            );
        }
        if seal && outcome.is_some() {
            ensure!(
                state.recovery_required.is_subset(&state.consumed),
                "recovery left unconsumed recorded outcomes"
            );
        }
        let outcome = outcome
            .map(|outcome| encode_outcome(&mut state, outcome, true))
            .transpose()?;
        if seal && let Some(expected) = &state.expected {
            ensure!(
                outcome.as_ref() == Some(expected),
                "replay final outcome divergence"
            );
        }
        state.sealed |= seal;
        Ok(TraceBundle {
            trace: CallTrace {
                version: 1,
                scope: state.scope.clone(),
                definition_hash: state.definition_hash.clone(),
                args_hash: state.args_hash.clone(),
                entries: state.entries.values().cloned().collect(),
                outcome,
            },
            blobs: state.blobs.values().cloned().collect(),
            memos: state.memos.clone(),
            observations: state.observations.iter().cloned().collect(),
        })
    }
}
fn add_observation(state: &mut TraceState, observation: TraceObservation) -> Result<()> {
    if state.observations.contains(&observation) {
        return Ok(());
    }
    let size = observation
        .definition_hash
        .len()
        .saturating_add(observation.op.len())
        .saturating_add(64);
    ensure!(
        size <= state
            .limits
            .metadata_bytes
            .saturating_sub(state.metadata_bytes),
        "trace metadata byte limit exceeded"
    );
    state.metadata_bytes += size;
    state.observations.insert(observation);
    Ok(())
}
fn reserve_blob(state: &mut TraceState, hash: &str, size: usize) -> Result<()> {
    if state.blobs.contains_key(hash) {
        return Ok(());
    }
    ensure!(
        size <= state.limits.blob_bytes.saturating_sub(state.blob_bytes),
        "trace encoded blob byte limit exceeded"
    );
    state.blob_bytes += size;
    Ok(())
}
fn reserve_entry(state: &mut TraceState, key: &TraceKey) -> Result<()> {
    ensure!(
        key.scope.len() <= loom_proto::TRACE_MAX_SCOPE_BYTES,
        "trace scope limit exceeded"
    );
    ensure!(
        state.entries.len() < state.limits.entries,
        "trace entry limit exceeded"
    );
    let size = key.scope.len() + 256;
    ensure!(
        size <= state
            .limits
            .metadata_bytes
            .saturating_sub(state.metadata_bytes),
        "trace metadata byte limit exceeded"
    );
    state.metadata_bytes += size;
    Ok(())
}
fn reserve_error(state: &mut TraceState, message: &str) -> Result<()> {
    ensure!(
        message.len() <= loom_proto::TRACE_MAX_ERROR_BYTES,
        "trace error message limit exceeded"
    );
    ensure!(
        message.len()
            <= state
                .limits
                .metadata_bytes
                .saturating_sub(state.metadata_bytes),
        "trace metadata byte limit exceeded"
    );
    state.metadata_bytes += message.len();
    Ok(())
}
fn error_message(error: &anyhow::Error) -> Result<String> {
    struct BoundedMessage {
        text: String,
    }
    impl std::fmt::Write for BoundedMessage {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            if text.len() > loom_proto::TRACE_MAX_ERROR_BYTES.saturating_sub(self.text.len()) {
                return Err(std::fmt::Error);
            }
            self.text.push_str(text);
            Ok(())
        }
    }
    let mut message = BoundedMessage {
        text: String::new(),
    };
    ensure!(
        std::fmt::write(&mut message, format_args!("{error:#}")).is_ok(),
        "trace error message limit exceeded"
    );
    Ok(message.text)
}
fn encode_outcome(
    state: &mut TraceState,
    outcome: &Result<EffectOutput>,
    root: bool,
) -> Result<TraceOutcome> {
    match outcome {
        Ok(output) => {
            let hash = blake3::hash(&output.bytes).to_hex().to_string();
            reserve_blob(state, &hash, output.bytes.len())?;
            state
                .blobs
                .entry(hash.clone())
                .or_insert_with(|| TraceBlob {
                    hash: hash.clone(),
                    kind: TraceBlobKind::Result,
                    bytes: output.bytes.clone(),
                });
            Ok(TraceOutcome::Success { result_hash: hash })
        }
        Err(error) => {
            let message = error_message(error)?;
            if root {
                ensure!(
                    message.len() <= loom_proto::TRACE_MAX_ERROR_BYTES,
                    "trace error message limit exceeded"
                );
            } else {
                reserve_error(state, &message)?;
            }
            Ok(TraceOutcome::Error { message })
        }
    }
}
impl EffectGuard {
    pub fn finish(mut self, outcome: &Result<EffectOutput>) -> Result<()> {
        let mut state = self.trace.state.lock().unwrap();
        let mut failure = None;
        if !state.sealed {
            let outcome = match encode_outcome(&mut state, outcome, false) {
                Ok(outcome) => outcome,
                Err(error) => {
                    let message = format!("{error:#}");
                    failure = Some(error);
                    TraceOutcome::Error { message }
                }
            };
            state
                .entries
                .get_mut(&self.key)
                .expect("active trace entry")
                .outcome = outcome;
            state.pending.remove(&self.key);
        }
        self.finished = true;
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
impl Drop for EffectGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.trace.state.lock().unwrap().pending.remove(&self.key);
        }
    }
}

pub(super) struct TraceSession {
    store: loom_store::Store,
    execution: Arc<ExecutionTrace>,
    finished: bool,
    recoverable: bool,
}
impl TraceSession {
    pub fn new(store: loom_store::Store, execution: Arc<ExecutionTrace>) -> Self {
        Self {
            store,
            execution,
            finished: false,
            recoverable: false,
        }
    }
    pub fn recoverable(mut self) -> Self {
        self.recoverable = true;
        self
    }
    pub fn finish(mut self, outcome: &Result<EffectOutput>) -> Result<()> {
        let _publication = self.execution.publication.lock().unwrap();
        let bundle = match self.execution.snapshot(Some(outcome), true) {
            Ok(bundle) => bundle,
            Err(error) => {
                self.finished = true;
                let failed = Err(anyhow::anyhow!("{error:#}"));
                if let Ok(bundle) = self.execution.snapshot(Some(&failed), true) {
                    self.store.persist_call_trace(&bundle)?;
                }
                return Err(error);
            }
        };
        self.finished = true;
        self.store.persist_call_trace(&bundle)?;
        Ok(())
    }
}
impl Drop for TraceSession {
    fn drop(&mut self) {
        if !self.finished {
            let _publication = self.execution.publication.lock().unwrap();
            if let Ok(mut bundle) = self.execution.snapshot(None, true) {
                if !self.recoverable {
                    bundle.trace.outcome = Some(TraceOutcome::Cancelled);
                }
                if let Err(error) = self.store.persist_call_trace(&bundle) {
                    eprintln!("persist cancelled execution trace: {error:#}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(
        trace: &Arc<ExecutionTrace>,
        scope: &str,
        occurrence: i64,
        desc: &Value,
        result: &Result<EffectOutput>,
    ) -> Result<()> {
        match trace.begin(scope, occurrence, desc)? {
            StartedEffect::Recorded(guard) => guard.finish(result)?,
            StartedEffect::Replayed(_) => bail!("unexpected replay"),
        }
        Ok(())
    }
    #[test]
    fn observations_are_deduplicated_per_executing_definition_and_survive_reload() -> Result<()> {
        let trace = ExecutionTrace::fresh("root");
        trace.observe("parent", "fs.list")?;
        trace.observe("parent", "fs.list")?;
        trace.observe("child", "fs.list")?;
        trace.observe("child", "fs.stat")?;
        let snapshot = trace.snapshot(None, false)?;
        assert_eq!(snapshot.observations.len(), 3);
        let reloaded = ExecutionTrace::loaded(snapshot.clone())?;
        reloaded.observe("parent", "fs.list")?;
        assert_eq!(
            reloaded.snapshot(None, false)?.observations,
            snapshot.observations
        );
        Ok(())
    }
    #[test]
    fn trace_limits_include_cancelled_entries_and_deduplicate_blobs() -> Result<()> {
        let trace = ExecutionTrace::fresh("root");
        let desc = json!({"op":"random"});
        let descriptor_size = super::super::encode(&desc)?.len();
        trace.state.lock().unwrap().limits = TraceLimits {
            entries: 2,
            blob_bytes: descriptor_size + 1,
            metadata_bytes: loom_proto::TRACE_MAX_METADATA_BYTES,
        };
        record(&trace, "root", 0, &desc, &EffectOutput::value(&Value::Null))?;
        let StartedEffect::Recorded(second) = trace.begin("root", 1, &desc)? else {
            bail!("expected fresh effect")
        };
        assert!(
            second
                .finish(&EffectOutput::value(&json!([1, 2, 3])))
                .is_err()
        );
        assert!(trace.begin("root", 2, &desc).is_err());
        let bundle = trace.snapshot(Some(&Err(anyhow::anyhow!("trace limit reached"))), true)?;
        assert_eq!(
            bundle
                .blobs
                .iter()
                .map(|blob| blob.bytes.len())
                .sum::<usize>(),
            descriptor_size + 1
        );
        assert!(matches!(
            bundle.trace.entries[1].outcome,
            TraceOutcome::Error { .. }
        ));
        let limits = TraceLimits {
            entries: 1,
            ..TraceLimits::default()
        };
        assert!(ExecutionTrace::loaded_with_limits(bundle.clone(), limits).is_err());
        let limits = TraceLimits {
            blob_bytes: descriptor_size,
            ..TraceLimits::default()
        };
        assert!(ExecutionTrace::loaded_with_limits(bundle, limits).is_err());
        let cancelled = ExecutionTrace::fresh("root");
        cancelled.state.lock().unwrap().limits.entries = 1;
        drop(cancelled.begin("root", 0, &desc)?);
        assert!(cancelled.begin("root", 1, &desc).is_err());
        assert!(matches!(
            cancelled.snapshot(None, true)?.trace.entries[0].outcome,
            TraceOutcome::Cancelled
        ));
        Ok(())
    }
    #[test]
    fn trace_rejects_overlong_keys_and_error_messages() -> Result<()> {
        let trace = ExecutionTrace::fresh("root");
        let desc = json!({"op":"unsupported"});
        assert!(
            trace
                .begin(
                    &"x".repeat(loom_proto::TRACE_MAX_SCOPE_BYTES + 1),
                    0,
                    &desc,
                )
                .is_err()
        );
        let StartedEffect::Recorded(guard) = trace.begin("root", 0, &desc)? else {
            bail!("fresh")
        };
        assert!(
            guard
                .finish(&Err(anyhow::anyhow!(
                    "x".repeat(loom_proto::TRACE_MAX_ERROR_BYTES + 1)
                )))
                .is_err()
        );
        let bundle = trace.snapshot(None, true)?;
        assert!(
            matches!(&bundle.trace.entries[0].outcome, TraceOutcome::Error { message } if message.len() < 128)
        );
        Ok(())
    }
    #[test]
    fn root_identity_is_checked_before_any_replayed_effect() -> Result<()> {
        let trace = ExecutionTrace::fresh("root");
        trace.identity("definition-a", &json!([1]))?;
        let result = EffectOutput::value(&Value::Null);
        let bundle = trace.snapshot(Some(&result), true)?;
        assert!(
            ExecutionTrace::loaded(bundle.clone())?
                .identity("definition-b", &json!([1]))
                .is_err()
        );
        assert!(
            ExecutionTrace::loaded(bundle.clone())?
                .identity("definition-a", &json!([2]))
                .is_err()
        );
        ExecutionTrace::loaded(bundle)?.identity("definition-a", &json!([1]))?;
        Ok(())
    }
    #[test]
    fn replay_distinguishes_occurrences_and_checks_effect_and_final_result() -> Result<()> {
        let trace = ExecutionTrace::fresh("root");
        let desc = json!({"op":"random"});
        record(
            &trace,
            "root/spawn:0",
            0,
            &desc,
            &EffectOutput::value(&json!(1)),
        )?;
        record(
            &trace,
            "root/spawn:0",
            1,
            &desc,
            &EffectOutput::value(&json!(2)),
        )?;
        let final_result = EffectOutput::value(&json!([1, 2]));
        let bundle = trace.snapshot(Some(&final_result), true)?;
        let replay = ExecutionTrace::loaded(bundle.clone())?;
        for occurrence in [1, 0] {
            match replay.begin("root/spawn:0", occurrence, &desc)? {
                StartedEffect::Replayed(output) => {
                    assert_eq!(output.decode()?, json!(occurrence + 1))
                }
                StartedEffect::Recorded(_) => bail!("replay executed effect"),
            }
        }
        assert!(
            replay
                .snapshot(Some(&EffectOutput::value(&json!(0))), true)
                .is_err()
        );
        replay.snapshot(Some(&final_result), true)?;
        let changed = ExecutionTrace::loaded(bundle)?;
        assert!(
            changed
                .begin("root/spawn:0", 0, &json!({"op":"now"}))
                .is_err()
        );
        assert!(changed.begin("root/spawn:0", 2, &desc).is_err());
        assert!(changed.snapshot(Some(&final_result), true).is_err());
        Ok(())
    }
    #[test]
    fn replay_retains_failure_and_recovery_retries_cancelled_effects() -> Result<()> {
        let trace = ExecutionTrace::fresh("actor:1");
        let fail = json!({"op":"unsupported"});
        let error = Err(anyhow::anyhow!("specific failure"));
        record(&trace, "actor:1", 0, &fail, &error)?;
        let waiting = trace.begin("actor:1", 1, &json!({"op":"sleep"}))?;
        drop(waiting);
        let partial = trace.snapshot(None, false)?;
        assert!(matches!(
            partial.trace.entries[1].outcome,
            TraceOutcome::Cancelled
        ));
        let recovered = ExecutionTrace::loaded(partial)?;
        let error = recovered
            .begin("actor:1", 0, &fail)
            .err()
            .context("recorded error missing")?;
        assert_eq!(error.to_string(), "specific failure");
        assert!(matches!(
            recovered.begin("actor:1", 1, &json!({"op":"sleep"}))?,
            StartedEffect::Recorded(_)
        ));
        Ok(())
    }
    #[test]
    fn cancelled_scoped_child_does_not_block_replay_completion() -> Result<()> {
        let original = ExecutionTrace::fresh("root");
        drop(original.begin("root/spawn:0", 0, &json!({"op":"sleep"}))?);
        let result = EffectOutput::value(&json!(1));
        let replay = ExecutionTrace::loaded(original.snapshot(Some(&result), true)?)?;
        assert!(replay.snapshot(Some(&result), true).is_err());
        replay.finish_scope("another-execution");
        assert!(replay.snapshot(Some(&result), true).is_err());
        replay.finish_scope("root");
        replay.snapshot(Some(&result), true)?;
        Ok(())
    }

    #[test]
    fn partial_recovery_cannot_complete_with_unconsumed_success_or_error() -> Result<()> {
        for failed in [false, true] {
            let trace = ExecutionTrace::fresh("actor:1");
            let descriptor = json!({"op":"random"});
            let result = if failed {
                Err(anyhow::anyhow!("saved failure"))
            } else {
                EffectOutput::value(&json!(5))
            };
            record(&trace, "actor:1", 0, &descriptor, &result)?;
            let recovered = ExecutionTrace::loaded(trace.snapshot(None, false)?)?;
            let final_result = EffectOutput::value(&Value::Null);
            assert!(recovered.snapshot(Some(&final_result), true).is_err());
            assert!(
                recovered.snapshot(None, false).is_ok(),
                "incomplete checkpoint remains legal"
            );
            let replayed = recovered.begin("actor:1", 0, &descriptor);
            if failed {
                assert_eq!(
                    replayed.err().context("error missing")?.to_string(),
                    "saved failure"
                );
            } else {
                assert!(matches!(replayed?, StartedEffect::Replayed(_)));
            }
            recovered.snapshot(Some(&final_result), true)?;
        }
        Ok(())
    }
}
