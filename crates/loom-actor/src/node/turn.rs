use super::*;

impl Node {
    pub(crate) async fn step(&self, id: &str, cancellation: &tokio::sync::Notify) -> Result<bool> {
        let admission = self.guard(&format!("lifecycle:{id}")).await;
        let actor = self.open_actor(id).await?;
        let mut conn = actor.conn.lock().await;
        if actor::status(&conn).await? != Status::Running || actor::meta(&conn, "ready").await? != "true" {
            return Ok(false);
        }
        drop(admission);
        if let Some(store) = &self.remote
            && let Err(error) = store.check(id)
        {
            self.archive_stale(id, &mut conn).await?;
            return Err(error);
        }
        let generation: i64 = actor::meta(&conn, "generation").await?.parse()?;
        let cursor = actor::cursor(&conn).await?;
        if self.config.io != crate::Io::Memory
            && cursor > 0
            && cursor % self.config.snapshot_every == 0
            && actor::query(&conn, "SELECT seq FROM inbox WHERE state='done' AND seq>? LIMIT 1", [cursor]).await?.rows.is_empty()
        {
            actor::snapshot(&conn, &self.snapshot_path(id, generation, cursor), cursor).await?;
        }
        let Some(message) = actor::next(&conn).await? else {
            return Ok(false);
        };
        let code = actor::code(&conn).await?;
        let behavior = actor::behavior(&self.registry, &code.hash)?;
        for retry in 0..=self.config.max_retries {
            match actor::attempt(
                &mut conn,
                id,
                &message,
                behavior.as_ref(),
                code.revision,
                &crate::effects::RuntimeEffects { node: self, external: self.effects.as_ref() },
                Some(&crate::durability::AttemptControl { node: self, cancellation }),
            )
            .await
            {
                Ok(false) => return Ok(false),
                Ok(true) => {
                    let cursor = actor::cursor(&conn).await?;
                    if self.config.io != crate::Io::Memory
                        && cursor > 0
                        && cursor % self.config.snapshot_every == 0
                        && actor::query(&conn, "SELECT seq FROM inbox WHERE state='done' AND seq>? LIMIT 1", [cursor])
                            .await?
                            .rows
                            .is_empty()
                    {
                        actor::snapshot(&conn, &self.snapshot_path(id, generation, cursor), cursor).await?;
                    }
                    return Ok(true);
                }
                Err(error) if error.durability => {
                    if self.remote.as_ref().is_some_and(|store| store.check(id).is_err()) {
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
                    return Ok(true);
                }
            }
        }
        Err(anyhow!("actor {id} seq {}: retry loop exhausted unexpectedly", message.seq))
    }

    pub(crate) async fn promote_inner(&self, id: &str, hash: &str, author: &str, rationale: &str) -> Result<()> {
        let actor = self.open_actor(id).await?;
        let behavior = actor::behavior(&self.registry, hash).with_context(|| format!("actor {id} seq -1: promote"))?;
        actor::promote(
            &mut *actor.conn.lock().await,
            behavior.as_ref(),
            author,
            rationale,
            &crate::effects::RuntimeEffects { node: self, external: self.effects.as_ref() },
            Some(self),
        )
        .await
        .with_context(|| format!("actor {id} seq -1: promote"))?;
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
        Ok(())
    }
}
