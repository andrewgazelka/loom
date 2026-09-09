use super::EffectOutput;
use anyhow::{Context, Result, bail, ensure};
use loom_proto::{CallTrace, TraceBlob, TraceBundle, TraceBlobKind, TraceMemo, TraceEntry, TraceKey, TraceOutcome, Value};
use std::{collections::{BTreeMap, BTreeSet}, sync::{Arc, Mutex}};

pub(super) struct ExecutionTrace {
    state: Mutex<TraceState>,
}
struct TraceState {
    scope: String,
    entries: BTreeMap<TraceKey, TraceEntry>,
    blobs: BTreeMap<String, TraceBlob>,
    pending: BTreeSet<TraceKey>,
    consumed: BTreeSet<TraceKey>,
    replay: bool,
    expected: Option<TraceOutcome>,
    memos: Vec<TraceMemo>,
    sealed: bool,
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
        Arc::new(Self { state: Mutex::new(TraceState {
            scope: scope.into(), entries: BTreeMap::new(), blobs: BTreeMap::new(),
            pending: BTreeSet::new(), consumed: BTreeSet::new(), replay: false, expected: None, memos: Vec::new(), sealed: false,
        }) })
    }
    pub fn loaded(bundle: TraceBundle) -> Result<Arc<Self>> {
        ensure!(bundle.trace.version == 1, "unsupported trace version");
        let trace = Self::fresh(&bundle.trace.scope);
        {
            let mut state = trace.state.lock().unwrap();
            state.replay = bundle.trace.outcome.is_some();
            state.expected = bundle.trace.outcome.clone();
            state.memos = bundle.memos;
            for blob in bundle.blobs {
                ensure!(blake3::hash(&blob.bytes).to_hex().as_str() == blob.hash, "trace blob hash mismatch");
                state.blobs.insert(blob.hash.clone(), blob);
            }
            for entry in bundle.trace.entries {
                ensure!(state.entries.insert(entry.key.clone(), entry).is_none(), "duplicate trace key");
            }
        }
        Ok(trace)
    }
    pub fn begin(self: &Arc<Self>, scope: &str, occurrence: i64, descriptor: &Value, subtree: bool) -> Result<StartedEffect> {
        let descriptor_bytes = super::encode(descriptor)?;
        let descriptor_hash = blake3::hash(&descriptor_bytes).to_hex().to_string();
        let key = TraceKey { scope: scope.into(), occurrence };
        let mut state = self.state.lock().unwrap();
        ensure!(!state.sealed, "execution trace already sealed");
        if let Some(entry) = state.entries.get(&key).cloned() {
            ensure!(entry.descriptor_hash == descriptor_hash, "replay descriptor divergence at {scope}:{occurrence}");
            ensure!(state.consumed.insert(key.clone()), "duplicate effect occurrence at {scope}:{occurrence}");
            if subtree {
                let prefix = format!("{scope}/race:{occurrence}");
                let descendants: Vec<_> = state.entries.keys().filter(|key| key.scope == prefix || key.scope.starts_with(&format!("{prefix}/"))).cloned().collect();
                state.consumed.extend(descendants);
            }
            return match entry.outcome {
                TraceOutcome::Success { result_hash } => {
                    let bytes = state.blobs.get(&result_hash).context("trace result blob missing")?.bytes.clone();
                    Ok(StartedEffect::Replayed(EffectOutput { bytes }))
                }
                TraceOutcome::Error { message } => bail!("{message}"),
                TraceOutcome::Cancelled if !state.replay => {
                    state.entries.remove(&key);
                    state.consumed.remove(&key);
                    drop(state);
                    self.begin(scope, occurrence, descriptor, subtree)
                }
                TraceOutcome::Cancelled => bail!("replayed cancelled effect at {scope}:{occurrence}"),
            };
        }
        ensure!(!state.replay, "replay missing effect at {scope}:{occurrence}");
        state.blobs.insert(descriptor_hash.clone(), TraceBlob { hash: descriptor_hash.clone(), kind: TraceBlobKind::Descriptor, bytes: descriptor_bytes });
        state.entries.insert(key.clone(), TraceEntry { key: key.clone(), descriptor_hash, outcome: TraceOutcome::Cancelled });
        state.pending.insert(key.clone());
        state.consumed.insert(key.clone());
        Ok(StartedEffect::Recorded(EffectGuard { trace: self.clone(), key, finished: false }))
    }
    pub fn memo(&self, descriptor_hash: String, result_hash: String) {
        self.state.lock().unwrap().memos.push(TraceMemo { descriptor_hash, scope: "global".into(), occurrence: 0, result_hash });
    }
    pub fn snapshot(&self, outcome: Option<&Result<EffectOutput>>, seal: bool) -> Result<TraceBundle> {
        let mut state = self.state.lock().unwrap();
        if state.replay && seal {
            ensure!(state.entries.len() == state.consumed.len(), "replay left unconsumed effects");
        }
        let outcome = outcome.map(|outcome| encode_outcome(&mut state, outcome));
        if seal && let Some(expected) = &state.expected { ensure!(outcome.as_ref() == Some(expected), "replay final outcome divergence"); }
        state.sealed |= seal;
        Ok(TraceBundle { trace: CallTrace { version: 1, scope: state.scope.clone(), entries: state.entries.values().cloned().collect(), outcome }, blobs: state.blobs.values().cloned().collect(), memos: state.memos.clone() })
    }
}
fn encode_outcome(state: &mut TraceState, outcome: &Result<EffectOutput>) -> TraceOutcome {
    match outcome {
        Ok(output) => {
            let hash = blake3::hash(&output.bytes).to_hex().to_string();
            state.blobs.entry(hash.clone()).or_insert_with(|| TraceBlob { hash: hash.clone(), kind: TraceBlobKind::Result, bytes: output.bytes.clone() });
            TraceOutcome::Success { result_hash: hash }
        }
        Err(error) => TraceOutcome::Error { message: format!("{error:#}") },
    }
}
impl EffectGuard {
    pub fn finish(mut self, outcome: &Result<EffectOutput>) {
        let mut state = self.trace.state.lock().unwrap();
        if !state.sealed {
            let outcome = encode_outcome(&mut state, outcome);
            state.entries.get_mut(&self.key).expect("active trace entry").outcome = outcome;
            state.pending.remove(&self.key);
        }
        self.finished = true;
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
    pub fn new(store: loom_store::Store, execution: Arc<ExecutionTrace>) -> Self { Self { store, execution, finished: false, recoverable: false } }
    pub fn recoverable(mut self) -> Self { self.recoverable = true; self }
    pub fn finish(mut self, outcome: &Result<EffectOutput>) -> Result<()> {
        let bundle = self.execution.snapshot(Some(outcome), true)?;
        self.finished = true;
        self.store.persist_call_trace(&bundle)?;
        Ok(())
    }
}
impl Drop for TraceSession {
    fn drop(&mut self) {
        if !self.finished {
            let outcome = Err(anyhow::anyhow!("execution cancelled before completion"));
            if let Ok(bundle) = self.execution.snapshot(if self.recoverable { None } else { Some(&outcome) }, true) {
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

    fn record(trace: &Arc<ExecutionTrace>, scope: &str, occurrence: i64, desc: &Value, result: &Result<EffectOutput>) -> Result<()> {
        match trace.begin(scope, occurrence, desc, false)? {
            StartedEffect::Recorded(guard) => guard.finish(result),
            StartedEffect::Replayed(_) => bail!("unexpected replay"),
        }
        Ok(())
    }
    #[test]
    fn replay_distinguishes_occurrences_and_checks_descriptor_and_final_result() -> Result<()> {
        let trace = ExecutionTrace::fresh("root");
        let desc = json!({"op":"random"});
        record(&trace, "root/all:0", 0, &desc, &EffectOutput::value(&json!(1)))?;
        record(&trace, "root/all:0", 1, &desc, &EffectOutput::value(&json!(2)))?;
        let final_result = EffectOutput::value(&json!([1, 2]));
        let bundle = trace.snapshot(Some(&final_result), true)?;
        let replay = ExecutionTrace::loaded(bundle.clone())?;
        for occurrence in [1, 0] {
            match replay.begin("root/all:0", occurrence, &desc, false)? {
                StartedEffect::Replayed(output) => assert_eq!(output.decode()?, json!(occurrence + 1)),
                StartedEffect::Recorded(_) => bail!("replay executed effect"),
            }
        }
        assert!(replay.snapshot(Some(&EffectOutput::value(&json!(0))), true).is_err());
        replay.snapshot(Some(&final_result), true)?;
        let changed = ExecutionTrace::loaded(bundle)?;
        assert!(changed.begin("root/all:0", 0, &json!({"op":"now"}), false).is_err());
        assert!(changed.begin("root/all:0", 2, &desc, false).is_err());
        assert!(changed.snapshot(Some(&final_result), true).is_err());
        Ok(())
    }
    #[test]
    fn replay_retains_failure_and_recovery_retries_cancelled_effects() -> Result<()> {
        let trace = ExecutionTrace::fresh("actor:1");
        let fail = json!({"op":"unsupported"});
        let error = Err(anyhow::anyhow!("specific failure"));
        record(&trace, "actor:1", 0, &fail, &error)?;
        let waiting = trace.begin("actor:1", 1, &json!({"op":"sleep"}), false)?;
        drop(waiting);
        let partial = trace.snapshot(None, false)?;
        assert!(matches!(partial.trace.entries[1].outcome, TraceOutcome::Cancelled));
        let recovered = ExecutionTrace::loaded(partial)?;
        let error = recovered.begin("actor:1", 0, &fail, false).err().context("recorded error missing")?;
        assert_eq!(error.to_string(), "specific failure");
        assert!(matches!(recovered.begin("actor:1", 1, &json!({"op":"sleep"}), false)?, StartedEffect::Recorded(_)));
        Ok(())
    }
    #[test]
    fn race_replay_consumes_cancelled_descendants_without_executing_them() -> Result<()> {
        let trace = ExecutionTrace::fresh("root");
        let race = json!({"op":"race","args":{"descs":[{"op":"sleep"},{"op":"random"}]}});
        let StartedEffect::Recorded(parent) = trace.begin("root", 0, &race, true)? else { bail!("not fresh") };
        let pending = trace.begin("root/race:0", 0, &json!({"op":"sleep"}), false)?;
        record(&trace, "root/race:0", 1, &json!({"op":"random"}), &EffectOutput::value(&json!(4)))?;
        drop(pending);
        let result = EffectOutput::value(&json!(4));
        parent.finish(&result);
        let replay = ExecutionTrace::loaded(trace.snapshot(Some(&result), true)?)?;
        assert!(matches!(replay.begin("root", 0, &race, true)?, StartedEffect::Replayed(_)));
        replay.snapshot(Some(&result), true)?;
        Ok(())
    }
}
