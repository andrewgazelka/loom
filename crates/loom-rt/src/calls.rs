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
    pub(super) async fn call_scoped(
        &self,
        hash: &str,
        args: Value,
        scope: &str,
        effects: EffectContext,
    ) -> Result<EffectOutput> {
        Ok(self
            .call_scoped_delegated(hash, args, scope, effects)
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
        execution.identity(hash, &args)?;
        let session = trace::TraceSession::new(self.inner.store.clone(), execution.clone());
        let effects = EffectContext {
            trace: Some(execution),
            ..EffectContext::default()
        };
        let result = self
            .core_call_entry(hash, entry, &args, scope, &effects)
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
    pub(super) async fn call_scoped_delegated(
        &self,
        hash: &str,
        args: Value,
        scope: &str,
        effects: EffectContext,
    ) -> Result<EncodedCall> {
        self.core_call(hash, &args, scope, &effects).await
    }
}
