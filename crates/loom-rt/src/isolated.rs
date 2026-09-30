//! The host side of an isolated call, in one place. Both entry points, the
//! `loom.call` core import (`sharedcore/linker.rs`) and the JavaScript
//! `{"op":"call"}` descriptor adapter (`root_handler.rs`), build a
//! `loom_proto::isolated::Request` and come here. Every decision lives in
//! `Runtime::isolated_call`: target resolution, effect policy, depth, arity
//! (inside `core_call_entry`), and the mapping of host failures onto
//! `CallError`. The payload is never decoded; only its hash is ever computed.
use super::*;
use loom_proto::isolated::{CallError, MAX_DEPTH, Request, Target};
use std::sync::atomic::AtomicUsize;

/// Most result bytes one `call_many` batch hands back: the reply a guest is asked to hold. A call
/// that would push the batch past it is that call's own `CallError::Trapped`, not a trap of the
/// whole execution.
pub(crate) const BATCH_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Result bytes a batch may hold before calls not yet started are refused: a memory backstop,
/// four responses' worth. Below it, the response budget is charged deterministically.
pub(crate) const BATCH_HOLD_BYTES: usize = 4 * BATCH_RESPONSE_BYTES;

/// Bytes a response frame adds to a result: a tag byte and a length prefix.
const RESPONSE_FRAME_OVERHEAD: usize = 5;

fn over_budget(request: &Request<'_>, what: &str) -> CallError {
    CallError::Trapped {
        hash: request.target.label(),
        message: format!("the batch's results exceed {BATCH_RESPONSE_BYTES} bytes: {what}"),
    }
}

/// Charge the batch's results against `budget` in request order: the first `Ok` that would take
/// the total past it, and every `Ok` after it, is replaced by an `Err` for its own position. A
/// failed call costs nothing. The outcome depends on the results alone, so a replay sees the same
/// calls fail.
fn charge_in_order(frames: &[&[u8]], outcomes: &mut [Result<Vec<u8>, CallError>], budget: usize) {
    let mut total = 0usize;
    let mut exhausted = false;
    for (frame, outcome) in frames.iter().zip(outcomes.iter_mut()) {
        let Ok(bytes) = outcome else {
            continue;
        };
        total = total.saturating_add(bytes.len() + RESPONSE_FRAME_OVERHEAD);
        exhausted |= total > budget;
        if exhausted && let Ok(request) = Request::parse(frame) {
            *outcome = Err(over_budget(&request, "this result was dropped"));
        }
    }
}

/// One identical pure call in flight. The callers that joined it (subscribed while it ran) wait on
/// it; the leader publishes its outcome into it, or drops it and leaves them with nothing.
pub(crate) type Flight = tokio::sync::watch::Sender<Option<Result<Vec<u8>, CallError>>>;

/// A run of a callee, and whether it was clean: it recorded no effect under its scope, no kernel
/// failed and no call was refused for depth while it ran, so its outcome depends on its inputs
/// alone. Only a clean outcome is cached or handed to other callers.
struct Ran {
    result: Result<Vec<u8>, CallError>,
    clean: bool,
    /// For a failed run: the failure as other callers may be given it, or `None` when it must
    /// not be shared (see [`shareable_failure`]).
    shareable: Option<CallError>,
}

/// The failure of a run as a caller that only waited for it may be given: the callee's own
/// deterministic failures (a decode or arity error, a missing definition, a trap in its own
/// code), with no text from the run that produced it. Host-side failures (an execution deadline,
/// cancellation, a missing or unbuildable artifact, an I/O error) belong to the leader's moment
/// and its scope, so they are not shared and each waiter makes its own attempt. A trap is shared
/// as the guest's own message, without the context the host adds (the leader's trace scope).
fn shareable_failure(hash: &str, error: &anyhow::Error) -> Option<CallError> {
    if let Some(error) = error.downcast_ref::<CallError>() {
        return matches!(
            error,
            CallError::Decode { .. } | CallError::Arity { .. } | CallError::NotFound { .. }
        )
        .then(|| error.clone());
    }
    if let Some(missing) = error.downcast_ref::<DefinitionNotFound>() {
        return Some(CallError::NotFound {
            hash: missing.hash.clone(),
        });
    }
    // A wasm trap is a `GuestFailure`, and so is the interrupt the host raises for a deadline or a
    // cancellation, which is the leader's moment and not the callee's behaviour.
    let guest = error.downcast_ref::<GuestFailure>()?;
    (!guest.message.contains("interrupt")).then(|| CallError::Trapped {
        hash: hash.into(),
        message: guest.message.clone(),
    })
}

/// The caller that runs a single-flight call. It owns the in-flight entry: the entry is removed
/// when the leader publishes, finishes, or is dropped mid-run (a cancelled caller or batch), and
/// only while it is still the leader's own (a later flight under the same key stays).
struct Leader<'a> {
    inflight: &'a Mutex<HashMap<InflightKey, Arc<Flight>>>,
    key: InflightKey,
    flight: Arc<Flight>,
}
impl Leader<'_> {
    fn remove(&self) {
        let mut inflight = self
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inflight
            .get(&self.key)
            .is_some_and(|kept| Arc::ptr_eq(kept, &self.flight))
        {
            inflight.remove(&self.key);
        }
    }
    /// Give `outcome` to the callers already waiting. The entry goes first, so a caller that
    /// arrives from now on starts a flight of its own instead of inheriting this outcome.
    fn publish(&self, outcome: &Result<Vec<u8>, CallError>) {
        self.remove();
        // Nobody can subscribe once the entry is gone, so the count is final: no waiter, no copy.
        if self.flight.receiver_count() > 0 {
            self.flight.send_replace(Some(outcome.clone()));
        }
    }
}
impl Drop for Leader<'_> {
    fn drop(&mut self) {
        self.remove();
    }
}

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
            self.inner.depth_refusals.fetch_add(1, Ordering::Relaxed);
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
                .await
                .result;
        }
        // Single flight: identical pure calls in flight at once run once. The first (the leader)
        // computes; the others wait on its flight and take its outcome exactly as a cache hit
        // would (no child trace of their own), but only a clean one: a run that recorded an effect
        // or saw a kernel failure is not evidence for anyone else (a waiter's policy may differ,
        // and its trace must hold its own record), so each waiter then runs its own attempt. A
        // deterministic failure of the callee is shared with the waiters that joined before it,
        // instead of being retried serially by each (a failure of the host, such as a deadline, is
        // not: see `shareable_failure`); the next separate request starts afresh.
        let key = (
            hash.clone(),
            request.entry.to_owned(),
            request.argc,
            *blake3::hash(request.payload).as_bytes(),
            kernels,
        );
        let leader = {
            let mut inflight = self.inner.inflight.lock().expect("inflight poisoned");
            if let Some(flight) = inflight.get(&key) {
                Err(flight.subscribe())
            } else {
                // The first receiver is dropped at once: the flight's receivers are its waiters.
                let (flight, _) = tokio::sync::watch::channel(None);
                let flight = Arc::new(flight);
                inflight.insert(key.clone(), flight.clone());
                Ok(Leader {
                    inflight: &self.inner.inflight,
                    key,
                    flight,
                })
            }
        };
        match leader {
            Err(mut waiting) => {
                // `changed` fails when the leader left without publishing; the value says which.
                let _ = waiting.changed().await;
                let shared = waiting.borrow().clone();
                match shared {
                    Some(outcome) => outcome,
                    None => {
                        self.run_isolated(
                            &hash, &request, scope, occurrence, effects, true, &kernels,
                        )
                        .await
                        .result
                    }
                }
            }
            Ok(leader) => {
                let ran = self
                    .run_isolated(&hash, &request, scope, occurrence, effects, true, &kernels)
                    .await;
                if ran.clean {
                    match (&ran.result, ran.shareable) {
                        (Ok(_), _) => leader.publish(&ran.result),
                        (Err(_), Some(failure)) => leader.publish(&Err(failure)),
                        (Err(_), None) => {}
                    }
                }
                ran.result
            }
        }
    }

    /// Run a batch of request frames concurrently as siblings of one execution: call `i` takes
    /// occurrence `base + i`, so each child's trace scope is fixed before any runs, and outcomes
    /// are returned in request order.
    ///
    /// Concurrency is lanes, each taking the next unstarted call when it is free, so one slow call
    /// holds up only its own lane. The first lane is always there; up to one per core in all
    /// (`width`) are added by taking a slot from the runtime-wide `batch_lanes` (four per core for
    /// every batch together) without waiting, and a lane gives its slot back when it runs out of
    /// calls, not when the batch ends. A batch that finds no slot runs its calls on the lane it
    /// has. Nothing in the host ever waits for a lane, and the unconditional lane always makes
    /// progress, so nested batches cannot deadlock however deep they go, while the live instances
    /// a fan-out creates are bounded by the slots plus one per level instead of `width ^ depth`.
    /// (A plain semaphore held around running a callee would deadlock: a parent sits inside
    /// `run_isolated` for as long as its children run, because the guest suspends in the host
    /// call. What a suspended parent releases is the execution's own guest slot, taken back in the
    /// `loom.call_many` import, not anything held here.)
    ///
    /// Results are budgeted to [`BATCH_RESPONSE_BYTES`] in request order: every call runs, then
    /// the results are charged index by index and the first one that does not fit, with every
    /// later one, becomes an `Err` for its own position (see [`charge_in_order`]). So which calls
    /// fail for size is a function of the results alone, not of completion order. The one
    /// exception is a backstop on memory: once [`BATCH_HOLD_BYTES`] of results are held, calls
    /// not yet started are not run.
    pub(crate) async fn isolated_batch(
        &self,
        frames: &[&[u8]],
        scope: &str,
        base: i64,
        effects: &EffectContext,
    ) -> Vec<Result<Vec<u8>, CallError>> {
        let width = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .max(2);
        // One entry per lane; only the extra lanes hold a slot, and each holds it for as long as
        // its own future lives.
        let mut lane_slots = vec![None];
        for _ in 1..width.min(frames.len()) {
            match self.inner.batch_lanes.clone().try_acquire_owned() {
                Ok(slot) => lane_slots.push(Some(slot)),
                Err(_) => break,
            }
        }
        let next = AtomicUsize::new(0);
        let held = AtomicUsize::new(0);
        let (next, held) = (&next, &held);
        let lanes = lane_slots.into_iter().map(|slot| async move {
            let _slot = slot;
            let mut done = Vec::new();
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(frame) = frames.get(index) else {
                    break;
                };
                let outcome = self
                    .batch_element(frame, scope, base + index as i64, effects, held)
                    .await;
                done.push((index, outcome));
            }
            done
        });
        let finished = futures::future::join_all(lanes).await;
        let mut outcomes: Vec<Option<Result<Vec<u8>, CallError>>> =
            (0..frames.len()).map(|_| None).collect();
        for (index, outcome) in finished.into_iter().flatten() {
            outcomes[index] = Some(outcome);
        }
        let mut outcomes: Vec<_> = outcomes
            .into_iter()
            .map(|outcome| outcome.expect("each index is taken by exactly one lane"))
            .collect();
        charge_in_order(frames, &mut outcomes, BATCH_RESPONSE_BYTES);
        outcomes
    }

    /// One call of a batch. `held` counts the result bytes the batch holds so far, for the memory
    /// backstop only; the response budget is charged afterwards by [`charge_in_order`].
    async fn batch_element(
        &self,
        frame: &[u8],
        scope: &str,
        occurrence: i64,
        effects: &EffectContext,
        held: &AtomicUsize,
    ) -> Result<Vec<u8>, CallError> {
        let request = Request::parse(frame)?;
        if held.load(Ordering::Relaxed) >= BATCH_HOLD_BYTES {
            return Err(over_budget(
                &request,
                "the batch holds too many result bytes; this call was not run",
            ));
        }
        let bytes = self
            .isolated_call(request, scope, occurrence, effects)
            .await?;
        // A result that cannot fit the response alone is dropped at once, whatever the others are.
        if bytes.len() + RESPONSE_FRAME_OVERHEAD > BATCH_RESPONSE_BYTES {
            return Err(over_budget(&request, "this result alone does not fit"));
        }
        held.fetch_add(bytes.len(), Ordering::Relaxed);
        Ok(bytes)
    }

    /// Instantiate the callee and run it; store the result when `cacheable` and the call
    /// did nothing but compute (the run is then `clean`).
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
    ) -> Ran {
        let entry = (!request.entry.is_empty()).then_some(request.entry);
        let child_scope = format!("{scope}/call:{occurrence}");
        let child = EffectContext {
            depth: effects.depth + 1,
            stream: None,
            ..effects.clone()
        };
        let started = Instant::now();
        let kernel_failures = self.kernel_failures();
        let depth_refusals = self.inner.depth_refusals.load(Ordering::Relaxed);
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
        let (result, shareable) = match outcome {
            Ok(call) => (Ok(call.output.bytes), None),
            Err(error) => {
                let shareable = shareable_failure(hash, &error);
                (Err(host_failure(hash, error)), shareable)
            }
        };
        // Clean: the call really did nothing but compute. The static row can undercount, the
        // trace cannot. A kernel failure (denied, missing blob, I/O) is recorded nowhere else and
        // depends on host state, so a call during which one happened is not clean either. Nor is
        // one during which a call below it was refused for depth: the callee may have turned that
        // refusal into a value, which then depends on how deep the caller sits. Another call's
        // failure or refusal in the same window only makes this conservative. Without a trace
        // there is nothing to prove it with.
        let clean = cacheable
            && self.kernel_failures() == kernel_failures
            && self.inner.depth_refusals.load(Ordering::Relaxed) == depth_refusals
            && effects
                .trace
                .as_ref()
                .is_some_and(|trace| !trace.has_effects_under(&child_scope));
        if clean && let Ok(bytes) = &result {
            self.inner.call_results.put(
                hash,
                request.entry,
                request.argc,
                request.payload,
                kernels,
                bytes,
                u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            );
        }
        Ran {
            result,
            clean,
            shareable,
        }
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
            definition
                .sig
                .exports
                .iter()
                .find(|export| export.name == entry)
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
