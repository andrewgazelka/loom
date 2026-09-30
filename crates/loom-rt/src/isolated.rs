//! The host side of an isolated call, in one place. Both entry points, the
//! `loom.call` core import (`sharedcore/linker.rs`) and the JavaScript
//! `{"op":"call"}` descriptor adapter (`root_handler.rs`), build a
//! `loom_proto::isolated::Request` and come here. Every decision lives in
//! `Runtime::isolated_call`: target resolution, effect policy, depth, arity
//! (inside `core_call_entry`), and the mapping of host failures onto
//! `CallError`. The payload is never decoded; only its hash is ever computed.
use super::*;
use loom_proto::isolated::{CallError, MAX_DEPTH, Request, Target};

/// A definition lookup that found nothing, typed so the isolated boundary
/// reports `CallError::NotFound` structurally instead of matching a message.
#[derive(Debug)]
pub(crate) struct DefinitionNotFound {
    pub hash: String,
}
impl std::fmt::Display for DefinitionNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "definition {:?} not found", self.hash)
    }
}
impl std::error::Error for DefinitionNotFound {}

/// Map a host failure from running the callee onto the caller's `CallError`.
/// A `CallError` raised anywhere below (the callee wrapper's decode failure, a
/// nested call's own error, an arity mismatch) passes through unchanged; a
/// missing definition becomes `NotFound`; everything else, guest traps and
/// host failures alike, is `Trapped` with the flattened cause.
fn host_failure(hash: &str, error: anyhow::Error) -> CallError {
    if let Some(error) = error.downcast_ref::<CallError>() {
        return error.clone();
    }
    if let Some(missing) = error.downcast_ref::<DefinitionNotFound>() {
        return CallError::NotFound {
            hash: missing.hash.clone(),
        };
    }
    CallError::Trapped {
        hash: hash.into(),
        message: format!("{error:#}"),
    }
}

impl Runtime {
    /// Run `request` as a nested isolated call of the execution described by
    /// `effects`, at `scope`/`occurrence`. Returns the callee's result bytes.
    ///
    /// Order of refusals, all before the callee is instantiated: an
    /// unresolvable `$self`, effect policy (recorded in the trace like any
    /// denied effect), a borrowed-root context (an actor turn cannot host a
    /// nested definition), depth, then the stored signature's arity.
    pub(crate) async fn isolated_call(
        &self,
        request: Request<'_>,
        scope: &str,
        occurrence: i64,
        effects: &EffectContext,
    ) -> Result<Vec<u8>, CallError> {
        let hash = match request.target {
            Target::This => effects.def_hash.clone().ok_or_else(|| CallError::Decode {
                message: "$self names no definition outside a definition execution".into(),
            })?,
            Target::Hash(digest) => Target::Hash(digest).label(),
        };
        if !effects.permits("call") {
            let denied = CallError::Denied {
                effect: "call".into(),
                hash: hash.clone(),
            };
            return Err(self.record_denied(&request, &hash, &denied, scope, occurrence, effects));
        }
        if effects.root.is_some() {
            return Err(CallError::Denied {
                effect: "call".into(),
                hash,
            });
        }
        if effects.depth >= MAX_DEPTH {
            return Err(CallError::DepthExceeded {
                depth: effects.depth,
            });
        }
        let entry = (!request.entry.is_empty()).then_some(request.entry);
        // A pure callee's answer for these arguments, when it was computed before.
        let cacheable = effects.trace.is_some() && self.callee_is_pure(&hash, request.entry);
        let kernels = self.kernel_fingerprint();
        if cacheable
            && let Some(bytes) = self.inner.call_results.get(
                &hash,
                request.entry,
                request.argc,
                request.payload,
                &kernels,
            )
        {
            return Ok(bytes);
        }
        let child_scope = format!("{scope}/call:{occurrence}");
        let child = EffectContext {
            depth: effects.depth + 1,
            ..effects.clone()
        };
        let started = Instant::now();
        let kernel_failures = self.kernel_failures();
        let outcome = self
            .core_call_entry(
                &hash,
                entry,
                request.argc,
                request.payload,
                &child_scope,
                &child,
            )
            .await;
        {
            let mut samples = self.inner.isolated_call_us.lock().unwrap();
            if samples.len() == 100_000 {
                samples.drain(..50_000);
            }
            samples.push(started.elapsed().as_secs_f64() * 1_000_000.0);
        }
        let result = outcome
            .map(|call| call.output.bytes)
            .map_err(|error| host_failure(&hash, error))?;
        // Stored only when the call really did nothing but compute: the static row
        // can undercount, the trace cannot.
        // A kernel failure (denied, missing blob, I/O) is recorded nowhere else and depends on
        // host state, so a call during which one happened is not stored. Another call's failure
        // in the same window only makes this conservative.
        if cacheable
            && self.kernel_failures() == kernel_failures
            && effects
                .trace
                .as_ref()
                .is_some_and(|trace| !trace.has_effects_under(&child_scope))
        {
            self.inner.call_results.put(
                &hash,
                request.entry,
                request.argc,
                request.payload,
                &kernels,
                &result,
                u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            );
        }
        Ok(result)
    }

    /// Whether the stored signature says `entry` of `hash` has a fully known
    /// effect row that is empty or names only `kernel` (pure native ops, which
    /// record nothing in the trace; the kernel versions are in the cache key). A missing definition, a JavaScript one, or an unnamed
    /// entry on a definition with several, is not pure for this purpose.
    fn callee_is_pure(&self, hash: &str, entry: &str) -> bool {
        let Ok(Some(definition)) = self.inner.store.executable_definition(hash) else {
            return false;
        };
        if definition.lang.is_v8() {
            return false;
        }
        let selected = if entry.is_empty() {
            (definition.sig.exports.len() == 1).then(|| &definition.sig.exports[0])
        } else {
            definition.sig.exports.iter().find(|export| export.name == entry)
        };
        selected.is_some_and(|export| {
            !export.effects.unknown && export.effects.labels.iter().all(|label| label == "kernel")
        })
    }

    /// A denied call is an effect the trace records (a permitted one is not:
    /// the callee's own effects are recorded under `{scope}/call:{occurrence}`
    /// instead). The descriptor names the header and the payload hash only.
    fn record_denied(
        &self,
        request: &Request<'_>,
        hash: &str,
        denied: &CallError,
        scope: &str,
        occurrence: i64,
        effects: &EffectContext,
    ) -> CallError {
        let Some(trace) = &effects.trace else {
            return denied.clone();
        };
        let descriptor = json!({"op":"call","args":{"def":hash,"entry":request.entry,"argc":request.argc,
            "payload":blake3::hash(request.payload).to_hex().to_string()}});
        match trace.begin(scope, occurrence, &descriptor) {
            Ok(trace::StartedEffect::Recorded(guard)) => {
                if let Err(error) = guard.finish(&Err(anyhow::anyhow!("{denied}"))) {
                    return CallError::Trapped {
                        hash: hash.into(),
                        message: format!("recording a denied isolated call: {error:#}"),
                    };
                }
                denied.clone()
            }
            // A recorded denial replays as `begin` failing with its message.
            Err(_) => denied.clone(),
            Ok(trace::StartedEffect::Replayed(_)) => CallError::Trapped {
                hash: hash.into(),
                message: "replay holds a success for a denied isolated call".into(),
            },
        }
    }
}

#[cfg(test)]
mod tests;
