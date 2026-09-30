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
        // A pure callee's answer for these arguments, when it was computed before.
        // A callee that uses kernels is served from the cache only to a caller that could have
        // run it: permissions are the intersection of caller and callee, hits included.
        let cacheable = effects.trace.is_some()
            && self
                .callee_purity(&hash, request.entry)
                .is_some_and(|uses_kernel| !uses_kernel || effects.permits("kernel"));
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
        if !cacheable {
            return self
                .run_isolated(&hash, &request, scope, occurrence, effects, false, &kernels)
                .await;
        }
        // Single flight: identical pure calls in flight at once run once. The first computes; the
        // others wait on the same cell and take its bytes exactly as a cache hit would (no child
        // trace of their own). A failed first run leaves the cell empty and the next waiter runs
        // its own attempt.
        let key = (
            hash.clone(),
            request.entry.to_owned(),
            request.argc,
            *blake3::hash(request.payload).as_bytes(),
            kernels,
        );
        let cell = self
            .inner
            .inflight
            .lock()
            .expect("inflight poisoned")
            .entry(key.clone())
            .or_default()
            .clone();
        let result = cell
            .get_or_try_init(|| {
                self.run_isolated(&hash, &request, scope, occurrence, effects, true, &kernels)
            })
            .await
            .cloned();
        {
            let mut inflight = self.inner.inflight.lock().expect("inflight poisoned");
            if inflight.get(&key).is_some_and(|kept| Arc::ptr_eq(kept, &cell)) {
                inflight.remove(&key);
            }
        }
        result
    }

    /// Run a batch of request frames concurrently (about one per core at a time) as siblings of
    /// one execution: call `i` takes occurrence `base + i`, so each child's trace scope is fixed
    /// before any runs, and outcomes are returned in request order.
    pub(crate) async fn isolated_batch(
        &self,
        frames: &[&[u8]],
        scope: &str,
        base: i64,
        effects: &EffectContext,
    ) -> Vec<Result<Vec<u8>, CallError>> {
        use futures::StreamExt;
        let width = std::thread::available_parallelism().map_or(4, |n| n.get());
        futures::stream::iter(0..frames.len())
            .map(|index| async move {
                match Request::parse(frames[index]) {
                    Ok(request) => {
                        self.isolated_call(request, scope, base + index as i64, effects)
                            .await
                    }
                    Err(error) => Err(error),
                }
            })
            .buffered(width)
            .collect()
            .await
    }

    /// Instantiate the callee and run it; store the result when `cacheable` and the call
    /// did nothing but compute.
    #[allow(clippy::too_many_arguments)]
    async fn run_isolated(
        &self,
        hash: &str,
        request: &Request<'_>,
        scope: &str,
        occurrence: i64,
        effects: &EffectContext,
        cacheable: bool,
        kernels: &[u8; 32],
    ) -> Result<Vec<u8>, CallError> {
        let entry = (!request.entry.is_empty()).then_some(request.entry);
        let child_scope = format!("{scope}/call:{occurrence}");
        let child = EffectContext {
            depth: effects.depth + 1,
            stream: None,
            ..effects.clone()
        };
        let started = Instant::now();
        let kernel_failures = self.kernel_failures();
        let outcome = self
            .core_call_entry(
                hash,
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
            .map_err(|error| host_failure(hash, error))?;
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
                hash,
                request.entry,
                request.argc,
                request.payload,
                kernels,
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
    /// `Some(uses_kernel)` when it is pure in that sense, `None` when it is not.
    fn callee_purity(&self, hash: &str, entry: &str) -> Option<bool> {
        let Ok(Some(definition)) = self.inner.store.executable_definition(hash) else {
            return None;
        };
        if definition.lang.is_v8() {
            return None;
        }
        let selected = if entry.is_empty() {
            (definition.sig.exports.len() == 1).then(|| &definition.sig.exports[0])
        } else {
            definition.sig.exports.iter().find(|export| export.name == entry)
        };
        let export = selected?;
        (!export.effects.unknown && export.effects.labels.iter().all(|label| label == "kernel"))
            .then(|| !export.effects.labels.is_empty())
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
