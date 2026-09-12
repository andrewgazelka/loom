use super::*;

impl Runtime {
    pub async fn spawn(&self, hash: &str, initial: Value) -> Result<Actor> {
        self.spawn_identified(hash, initial, uuid::Uuid::new_v4().to_string())
            .await
    }
    pub(super) async fn spawn_identified(
        &self,
        hash: &str,
        initial: Value,
        id: String,
    ) -> Result<Actor> {
        if let Some(actor) = self.inner.store.actor(&id)? {
            return Ok(actor);
        }
        // Instantiate before publishing an actor so missing imports fail immediately.
        self.core_execute(
            hash,
            "actor.spawn",
            &EffectContext::default(),
            true,
            sharedcore::Entry::Validate,
        )
        .await?;
        let def = self
            .inner
            .store
            .definition(hash)?
            .context("built definition disappeared")?;
        let actor = Actor {
            id,
            behavior_hash: hash.into(),
            lang: def.lang,
            component_hash: def.component_hash,
            last_seq: 0,
            created_seq: 0,
            parent: None,
        };
        self.inner.store.create_initialized_actor(&actor, &initial)
    }
    pub async fn state(&self, actor: &str) -> Result<Value> {
        let actor = self.inner.store.actor(actor)?.context("actor not found")?;
        let snapshot = self
            .inner
            .store
            .latest_snapshot(&actor.id, &actor.behavior_hash)?;
        let mut seq = snapshot.as_ref().map_or(0, |s| s.seq);
        let mut state = snapshot.map_or(Value::Null, |s| s.state);
        loop {
            let events = self.inner.store.events(Some(&actor.id), seq, 1000)?;
            if events.is_empty() {
                break;
            }
            for event in events {
                seq = event.seq;
                if event.handler_seq == 0
                    && let Some(initial) = event.event.get("__loom_init")
                {
                    state = initial.clone();
                    continue;
                }
                state = self
                    .core_execute(
                        &actor.behavior_hash,
                        "fold",
                        &EffectContext::default(),
                        true,
                        sharedcore::Entry::Fold {
                            state: &state,
                            event: &event.event,
                        },
                    )
                    .await?
                    .output
                    .decode()?;
            }
        }
        self.inner
            .store
            .snapshot(&actor.id, &actor.behavior_hash, seq, &state)?;
        Ok(state)
    }
    pub async fn send(&self, actor: &str, msg: Value) -> Result<Value> {
        let message = self.inner.store.enqueue(actor, &msg)?;
        self.drain_actor(actor, Some(message.handler_seq)).await?;
        self.state(actor).await
    }
    pub fn enqueue(&self, actor: &str, msg: Value) -> Result<Value> {
        let message = self.inner.store.enqueue(actor, &msg)?;
        self.schedule_message(message)
    }
    pub(super) fn schedule_message(&self, message: loom_store::PendingMessage) -> Result<Value> {
        let runtime = self.clone();
        let actor = message.actor.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime.drain_actor(&actor, None).await {
                let _ = runtime.inner.store.append(
                    "system",
                    &json!({"type":"handler_failed","actor":actor,"error":format!("{error:#}")}),
                    0,
                );
            }
        });
        Ok(json!({"actor":message.actor,"handler_seq":message.handler_seq}))
    }
    pub(super) async fn drain_actor(&self, actor: &str, until: Option<i64>) -> Result<()> {
        let lock = self
            .inner
            .actor_locks
            .lock()
            .unwrap()
            .entry(actor.into())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        loop {
            let Some(message) = self.inner.store.pending(actor)? else {
                break;
            };
            if until.is_some_and(|seq| message.handler_seq > seq) {
                break;
            }
            let sequence = message.handler_seq;
            self.deliver(message).await?;
            if until == Some(sequence) {
                break;
            }
        }
        Ok(())
    }
    pub(super) async fn deliver(&self, message: loom_store::PendingMessage) -> Result<()> {
        let actor = &message.actor;
        let metadata = self.inner.store.actor(actor)?.context("actor not found")?;
        let state = self.state(actor).await?;
        let handler_seq = message.handler_seq;
        let scope = format!("{actor}:{handler_seq}");
        let execution = match self.inner.store.load_call_trace(&scope)? {
            Some(bundle) => trace::ExecutionTrace::loaded(bundle)?,
            None => trace::ExecutionTrace::fresh(&scope),
        };
        execution.identity(
            &metadata.behavior_hash,
            &json!({"state":state,"message":message.msg}),
        )?;
        let session =
            trace::TraceSession::new(self.inner.store.clone(), execution.clone()).recoverable();
        let effects = EffectContext {
            trace: Some(execution),
            actor_id: Some(actor.clone()),
            ..EffectContext::default()
        };
        let bytes = self
            .core_execute(
                &metadata.behavior_hash,
                &scope,
                &effects,
                false,
                sharedcore::Entry::Run {
                    state: &state,
                    message: &message.msg,
                },
            )
            .await?
            .output
            .bytes;
        let events = decode(&bytes)?
            .as_array()
            .context("run must return array of events")?
            .clone();
        session.finish(&Ok(EffectOutput { bytes }))?;
        self.inner
            .store
            .complete_message(actor, handler_seq, &events)?;
        Ok(())
    }
    pub async fn recover_pending(&self) -> Result<usize> {
        let pending = self.inner.store.pending_messages()?;
        let count = pending.len();
        let actors = pending
            .into_iter()
            .map(|message| message.actor)
            .collect::<std::collections::BTreeSet<_>>();
        for actor in actors {
            self.drain_actor(&actor, None).await?;
        }
        Ok(count)
    }
    pub async fn fork_actor(&self, actor: &str) -> Result<Actor> {
        let lock = self
            .inner
            .actor_locks
            .lock()
            .unwrap()
            .entry(actor.into())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        let original = self.inner.store.actor(actor)?.context("actor not found")?;
        let mut initial = Value::Null;
        let mut history = Vec::new();
        let mut after = 0;
        loop {
            let events = self.inner.store.events(Some(actor), after, 1000)?;
            if events.is_empty() {
                break;
            }
            for event in events {
                after = event.seq;
                if event.handler_seq == 0
                    && let Some(state) = event.event.get("__loom_init")
                {
                    initial = state.clone();
                } else {
                    history.push(event.event);
                }
            }
        }
        let mut fork = self.spawn(&original.behavior_hash, initial).await?;
        fork.parent = Some(actor.into());
        self.inner.store.update_actor(&fork)?;
        self.inner.store.append_batch(&fork.id, &history, 1)?;
        self.state(&fork.id).await?;
        self.inner
            .store
            .actor(&fork.id)?
            .context("fork disappeared")
    }
    pub async fn upgrade(&self, actor: &str, hash: &str) -> Result<Value> {
        let lock = self
            .inner
            .actor_locks
            .lock()
            .unwrap()
            .entry(actor.into())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        if self.inner.store.pending(actor)?.is_some() {
            bail!("cannot upgrade an actor with a pending message");
        }
        let mut actor = self.inner.store.actor(actor)?.context("actor not found")?;
        self.core_execute(
            hash,
            "upgrade",
            &EffectContext::default(),
            true,
            sharedcore::Entry::Validate,
        )
        .await?;
        let def = self
            .inner
            .store
            .definition(hash)?
            .context("built definition disappeared")?;
        actor.behavior_hash = hash.into();
        actor.component_hash = def.component_hash;
        self.inner.store.update_actor(&actor)?;
        self.state(&actor.id).await
    }
}
