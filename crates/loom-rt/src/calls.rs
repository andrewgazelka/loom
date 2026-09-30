//! Host `Value` API over definitions. Arguments arrive as a JSON array and
//! leave as the canonical DAG-CBOR payload the callee decodes; results come
//! back as bytes and decode to a `Value` here. Both are the host boundary's
//! own codec passes; guest-to-guest isolated calls never take this path.
use super::*;

impl Runtime {
    pub async fn call_def(&self, hash: &str, args: Value) -> Result<Value> {
        Ok(self.call_def_timed(hash, args).await?.value)
    }
    pub async fn call_def_timed(&self, hash: &str, args: Value) -> Result<TimedCall> {
        self.call_scoped_timed(hash, args, &format!("call:{}", uuid::Uuid::new_v4()))
            .await
    }
    pub async fn call_entry_timed(
        &self,
        hash: &str,
        entry: &str,
        args: Value,
    ) -> Result<TimedCall> {
        let scope = format!("call:{}", uuid::Uuid::new_v4());
        self.call_traced_entry_timed(
            hash,
            Some(entry),
            args,
            &scope,
            trace::ExecutionTrace::fresh(&scope),
        )
        .await
    }
    /// [`Self::call_entry_timed`] that also returns, for every effect the entry performed, the
    /// module offsets of the guest frames that performed it (innermost first; map them to source
    /// lines with the module's DWARF). Captures a backtrace per effect: for inspection, not the
    /// hot path.
    pub async fn call_entry_sites(
        &self,
        hash: &str,
        entry: &str,
        args: Value,
    ) -> Result<(TimedCall, Vec<Vec<u64>>)> {
        let scope = format!("call:{}", uuid::Uuid::new_v4());
        let sites: SiteLog = Arc::default();
        let (call, _) = self
            .call_traced_entry_inner(
                hash,
                Some(entry),
                args,
                &scope,
                trace::ExecutionTrace::fresh(&scope),
                None,
                Some(sites.clone()),
            )
            .await?;
        let lines = std::mem::take(&mut *sites.lock().unwrap());
        Ok((call, lines))
    }
    /// Whether a call of `entry` of `hash` may be answered from, and stored in, the result cache: the
    /// callee is pure (no effect but `kernel`) and, if it uses kernels, its own policy lets it.
    fn embedder_cacheable(&self, hash: &str, entry: &str) -> bool {
        let Some(uses_kernel) = self.callee_purity(hash, entry) else {
            return false;
        };
        let allowed = self
            .inner
            .store
            .executable_definition(hash)
            .ok()
            .flatten()
            .and_then(|definition| definition.allowed_effects);
        // A policy that refuses `kernel` must see the call run (and be refused), never a stored answer.
        !uses_kernel || allowed.is_none_or(|labels| labels.iter().any(|label| label == "kernel"))
    }

    /// Run one entry for an embedder, answered from the result cache when the callee is pure (no effect
    /// but `kernel`) and these exact arguments were computed before under the same kernel versions. A
    /// miss runs the call and stores its result only if the run was clean (see below), so the next
    /// identical call costs a lookup. `entry` is empty for a definition with one export.
    ///
    /// Clean, as on the isolated path: no effect recorded in the trace, no kernel failure and no depth
    /// refusal during the run. The static row can undercount and a callee can turn a failure into a value,
    /// which then depends on host state, so such a result is returned but never stored.
    pub async fn call_entry_cached(&self, hash: &str, entry: &str, args: Value) -> Result<CachedCall> {
        let (argc, payload) = positional_payload(&args)?;
        let kernels = self.kernel_fingerprint();
        let cacheable = self.embedder_cacheable(hash, entry);
        let started = Instant::now();
        if cacheable
            && let Some(bytes) = self.inner.call_results.get(hash, entry, argc, &payload, &kernels)
            && let Ok(value) = loom_proto::decode_host::<Value>(&bytes)
        {
            return Ok(CachedCall { value, cache_hit: true, run_ms: elapsed_ms(started) });
        }
        let scope = format!("call:{}", uuid::Uuid::new_v4());
        let trace = trace::ExecutionTrace::fresh(&scope);
        let kernel_failures = self.kernel_failures();
        let depth_refusals = self.inner.depth_refusals.load(Ordering::Relaxed);
        let (call, bytes) = self
            .call_traced_entry_inner(
                hash,
                (!entry.is_empty()).then_some(entry),
                args,
                &scope,
                trace.clone(),
                None,
                None,
            )
            .await?;
        let cost = started.elapsed();
        let clean = cacheable
            && self.kernel_failures() == kernel_failures
            && self.inner.depth_refusals.load(Ordering::Relaxed) == depth_refusals
            && !trace.has_effects_under(&scope);
        if clean {
            self.inner
                .call_results
                .put(hash, entry, argc, &payload, &kernels, &bytes, cost.as_nanos() as u64);
        }
        Ok(CachedCall { value: call.value, cache_hit: false, run_ms: cost.as_secs_f64() * 1000.0 })
    }

    /// [`Self::call_entry_cached`] over a batch, `parallel` at a time. Identical calls of a pure callee
    /// in one batch run once; every other call runs on its own, as a sequence of `run`s would. One result
    /// per call, in order; a failure is that call's alone.
    pub async fn call_many_cached(
        &self,
        calls: Vec<(String, String, Value)>,
        parallel: usize,
    ) -> Vec<Result<CachedCall>> {
        use futures::StreamExt;
        let mut unique: Vec<usize> = Vec::new();
        let mut first_of: HashMap<(String, String, String), usize> = HashMap::new();
        let mut owner = Vec::with_capacity(calls.len());
        let mut repeats = Vec::with_capacity(calls.len());
        for (index, (hash, entry, args)) in calls.iter().enumerate() {
            let shareable = self.embedder_cacheable(hash, entry);
            let mut repeat = true;
            let slot = if shareable {
                let key = (hash.clone(), entry.clone(), args.to_string());
                *first_of.entry(key).or_insert_with(|| {
                    repeat = false;
                    unique.push(index);
                    unique.len() - 1
                })
            } else {
                repeat = false;
                unique.push(index);
                unique.len() - 1
            };
            owner.push(slot);
            repeats.push(repeat);
        }
        let calls = Arc::new(calls);
        let mut outcomes: Vec<Option<Result<CachedCall, String>>> = (0..unique.len()).map(|_| None).collect();
        let mut running = futures::stream::iter(unique.iter().copied().enumerate().map(|(slot, index)| {
            let calls = calls.clone();
            async move {
                let (hash, entry, args) = &calls[index];
                (slot, self.call_entry_cached(hash, entry, args.clone()).await.map_err(|e| format!("{e:#}")))
            }
        }))
        .buffer_unordered(parallel.clamp(1, 256));
        while let Some((slot, outcome)) = running.next().await {
            outcomes[slot] = Some(outcome);
        }
        owner
            .into_iter()
            .zip(repeats)
            .map(|(slot, repeat)| match outcomes[slot].as_ref().expect("every unique call ran") {
                // A repeat inside the batch took the first call's answer: nothing ran for it either.
                Ok(call) => Ok(CachedCall {
                    value: call.value.clone(),
                    cache_hit: call.cache_hit || repeat,
                    run_ms: if repeat { 0.0 } else { call.run_ms },
                }),
                Err(message) => Err(anyhow::anyhow!("{message}")),
            })
            .collect()
    }

    /// A borrowed-effects call (`call_with_effects`): the caller owns the
    /// root handler, so no trace identity is recorded here.
    pub(super) async fn call_scoped(
        &self,
        hash: &str,
        args: Value,
        scope: &str,
        effects: EffectContext,
    ) -> Result<EffectOutput> {
        let (argc, payload) = positional_payload(&args)?;
        Ok(self
            .core_call_entry(hash, None, argc, &payload, scope, &effects)
            .await?
            .output)
    }
    pub(super) async fn call_scoped_timed(
        &self,
        hash: &str,
        args: Value,
        scope: &str,
    ) -> Result<TimedCall> {
        self.call_traced_timed(hash, args, scope, trace::ExecutionTrace::fresh(scope))
            .await
    }
    pub async fn replay_def_timed(
        &self,
        hash: &str,
        args: Value,
        scope: &str,
    ) -> Result<TimedCall> {
        let lock = self.trace_lock(scope);
        let _guard = lock.lock().await;
        let bundle = self
            .inner
            .store
            .load_call_trace(scope)?
            .context("call trace not found")?;
        anyhow::ensure!(
            bundle.trace.outcome.is_some(),
            "cannot explicitly replay an incomplete call trace"
        );
        anyhow::ensure!(
            !matches!(
                bundle.trace.outcome,
                Some(loom_proto::TraceOutcome::Cancelled)
            ),
            "recorded execution was cancelled before completion"
        );
        let execution = trace::ExecutionTrace::loaded(bundle)?;
        self.call_traced_timed(hash, args, scope, execution).await
    }
    pub(super) async fn call_traced_timed(
        &self,
        hash: &str,
        args: Value,
        scope: &str,
        execution: Arc<trace::ExecutionTrace>,
    ) -> Result<TimedCall> {
        self.call_traced_entry_timed(hash, None, args, scope, execution)
            .await
    }
    async fn call_traced_entry_timed(
        &self,
        hash: &str,
        entry: Option<&str>,
        args: Value,
        scope: &str,
        execution: Arc<trace::ExecutionTrace>,
    ) -> Result<TimedCall> {
        self.call_traced_entry_streaming(hash, entry, args, scope, execution, None)
            .await
    }
    /// [`Self::call_traced_entry_timed`], with `stream` receiving what the entry yields.
    pub(super) async fn call_traced_entry_streaming(
        &self,
        hash: &str,
        entry: Option<&str>,
        args: Value,
        scope: &str,
        execution: Arc<trace::ExecutionTrace>,
        stream: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    ) -> Result<TimedCall> {
        self.call_traced_entry_inner(hash, entry, args, scope, execution, stream, None)
            .await
            .map(|(call, _)| call)
    }
    #[allow(clippy::too_many_arguments)]
    async fn call_traced_entry_inner(
        &self,
        hash: &str,
        entry: Option<&str>,
        args: Value,
        scope: &str,
        execution: Arc<trace::ExecutionTrace>,
        stream: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
        sites: Option<SiteLog>,
    ) -> Result<(TimedCall, Vec<u8>)> {
        let (argc, payload) = positional_payload(&args)?;
        execution.identity(hash, &payload)?;
        let session = trace::TraceSession::new(self.inner.store.clone(), execution.clone(), entry);
        let effects = EffectContext {
            trace: Some(execution),
            stream,
            sites,
            ..EffectContext::default()
        };
        let result = self
            .core_call_entry(hash, entry, argc, &payload, scope, &effects)
            .await;
        let outcome = match &result {
            Ok(call) => Ok(call.output.clone()),
            Err(error) => Err(anyhow::anyhow!("{error:#}")),
        };
        session.finish(&outcome)?;
        let call = result?;
        Ok((
            TimedCall {
                scope: scope.into(),
                value: call.output.decode()?,
                timing: call.timing,
            },
            call.output.bytes,
        ))
    }
}
