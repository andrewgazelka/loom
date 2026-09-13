use super::*;

pub(crate) struct Batch {
    pub processed: usize,
    pub again: bool,
}

impl Node {
    /// One connection-lock scope owns admission and immutable execution metadata.
    /// End the batch before poison/control writes or releasing the lock; the next
    /// batch reloads them. Each successful message still commits independently.
    pub(crate) async fn step(&self, id: &str, cancellation: &tokio::sync::Notify) -> Result<Batch> {
        let admission = self.guard(&format!("lifecycle:{id}")).await;
        let actor = self.open_actor(id).await?;
        let mut conn = actor.conn.lock().await;
        let mut batch = Batch { processed: 0, again: false };
        if actor::status(&conn).await? != Status::Running {
            self.scheduling()?.index_dirty.insert(id.into());
            return Ok(batch);
        }
        if actor::meta(&conn, "ready").await? != "true" {
            return Ok(batch);
        }
        drop(admission);
        if let Err(error) = self.check_lease(id) {
            self.archive_stale(id, &mut conn).await?;
            return Err(error);
        }
        let schema = Self::schema_fingerprint(&conn).await?;
        self.snapshot_schema_change(&conn, id, &schema).await?;
        let generation: i64 = actor::meta(&conn, "generation").await?.parse()?;
        let cursor = actor::cursor(&conn).await?;
        if cursor > 0
            && cursor % self.config.snapshot_every == 0
            && actor::query(&conn, crate::mailbox::DONE_ABOVE, [cursor]).await?.rows.is_empty()
        {
            self.snapshot_actor(&conn, id, cursor).await?;
        }
        let code = actor::code(&conn).await?;
        let behavior = crate::view::behavior_on(&self.registry, &conn, &code.hash).await?;
        let mut state =
            actor::AttemptState { generation, revision: code.revision, epoch: actor::meta(&conn, "commit_epoch").await?.parse()? };
        for _ in 0..self.config.batch_limit {
            let Some(message) = crate::mailbox::next_at(&conn, state.epoch).await? else {
                return Ok(batch);
            };
            for retry in 0..=self.config.max_retries {
                match actor::attempt(
                    &mut conn,
                    id,
                    &message,
                    behavior.as_ref(),
                    &state,
                    &crate::effects::RuntimeEffects { node: self, external: self.effects.as_ref() },
                    Some(&crate::durability::AttemptControl { node: self, cancellation }),
                )
                .await
                {
                    Ok(actor::Attempt::Cancelled) => return Ok(batch),
                    Ok(actor::Attempt::Deferred) => {
                        batch.processed += 1;
                        // Retry through admission and pump, retaining selective receive
                        // and the second-deferral-without-a-commit trap.
                        batch.again = true;
                        return Ok(batch);
                    }
                    Ok(actor::Attempt::Complete { completion }) => {
                        state.epoch = completion.epoch;
                        batch.processed += 1;
                        self.fanout_on(id, &mut conn).await?;
                        self.snapshot_schema_change(&conn, id, &schema).await?;
                        self.record_send_outcome(
                            crate::send_outcome::MessageIdentity { id: id.into(), generation, seq: message.seq },
                            crate::SendOutcome::Complete { id: id.into(), seq: message.seq, cursor: completion.cursor },
                        )?;
                        if completion.cursor > 0 && completion.cursor % self.config.snapshot_every == 0 && completion.boundary {
                            self.snapshot_actor(&conn, id, completion.cursor).await?;
                        }
                        break;
                    }
                    Err(error) if error.durability => {
                        if self.check_lease(id).is_err() {
                            self.archive_stale(id, &mut conn).await?;
                        }
                        return Err(error.into());
                    }
                    Err(error) if error.runtime && retry < self.config.max_retries => {
                        tokio::time::sleep(self.config.retry_backoff.saturating_mul(u32::try_from(retry + 1)?)).await;
                    }
                    Err(error) => {
                        actor::poison(
                            &mut conn,
                            &message,
                            &error.message,
                            behavior.as_ref(),
                            &crate::effects::RuntimeEffects { node: self, external: self.effects.as_ref() },
                            self,
                        )
                        .await?;
                        self.record_send_outcome(
                            crate::send_outcome::MessageIdentity { id: id.into(), generation, seq: message.seq },
                            crate::SendOutcome::Failed {
                                id: id.into(),
                                seq: message.seq,
                                cursor: actor::cursor(&conn).await?,
                                cause: error.message,
                            },
                        )?;
                        batch.processed += 1;
                        batch.again = actor::status(&conn).await? == Status::Running;
                        if !batch.again {
                            self.scheduling()?.index_dirty.insert(id.into());
                        }
                        return Ok(batch);
                    }
                }
            }
        }
        batch.again = true;
        Ok(batch)
    }

    pub(crate) async fn promote_inner(&self, id: &str, hash: &str, author: &str, rationale: &str) -> Result<()> {
        let actor = self.open_actor(id).await?;
        let mut conn = actor.conn.lock().await;
        let behavior =
            crate::view::behavior_on(&self.registry, &conn, hash).await.with_context(|| format!("actor {id} seq -1: promote"))?;
        let schema = Self::schema_fingerprint(&conn).await?;
        actor::promote(
            &mut conn,
            behavior.as_ref(),
            author,
            rationale,
            &crate::effects::RuntimeEffects { node: self, external: self.effects.as_ref() },
            Some(self),
        )
        .await
        .with_context(|| format!("actor {id} seq -1: promote"))?;
        self.snapshot_schema_change(&conn, id, &schema).await?;
        drop(conn);
        self.scheduling()?.index_dirty.insert(id.into());
        self.wake_actor(id)?;
        self.sync_index(id).await
    }

    pub(super) async fn skip_inner(&self, id: &str) -> Result<()> {
        let actor = self.open_actor(id).await?;
        let mut conn = actor.conn.lock().await;
        let tx = conn.transaction().await?;
        ensure!(actor::status(&tx).await? == Status::Parked, "actor {id} seq -1: skip requires parked status");
        let message = actor::next(&tx).await?.context(format!("actor {id} seq -1: no message to skip"))?;
        crate::mailbox::complete(&tx, message.seq).await?;
        actor::set_meta(&tx, &format!("skipped:{}", message.seq), "1").await?;
        actor::set_meta(&tx, &format!("code_at:{}", message.seq), &actor::code(&tx).await?.revision.to_string()).await?;
        actor::set_meta(&tx, "status", "running").await?;
        self.commit_control(id, tx).await?;
        self.wake_actor(id)?;
        Ok(())
    }
}
