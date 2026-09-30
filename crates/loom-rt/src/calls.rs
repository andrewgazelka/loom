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
        let call = self
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
    ) -> Result<TimedCall> {
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
        Ok(TimedCall {
            scope: scope.into(),
            value: call.output.decode()?,
            timing: call.timing,
        })
    }
}
