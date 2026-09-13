//! Lifecycle hooks share the actor transaction and use a separate effect sequence.
use crate::{Behavior, Ctx, EffectHandler, Status, Trap, actor};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{future::Future, panic::AssertUnwindSafe, task::Poll};
use turso::Connection;

#[derive(Serialize, Deserialize)]
struct Termination {
    epoch: i64,
    counter: i64,
    revision: i64,
    behavior_hash: String,
    reason: String,
    trapped: bool,
}

async fn record_termination(conn: &Connection, behavior: &dyn Behavior, reason: &str, trapped: bool) -> Result<()> {
    let operation = Termination {
        epoch: actor::meta(conn, "commit_epoch").await?.parse()?,
        counter: actor::meta(conn, "hook_counter").await?.parse()?,
        revision: actor::code(conn).await?.revision,
        behavior_hash: behavior.hash().to_owned(),
        reason: reason.to_owned(),
        trapped,
    };
    actor::set_meta(conn, &format!("termination:{}", operation.counter), &serde_json::to_string(&operation)?).await
}

enum Hook<'a> {
    Terminate { reason: &'a str },
    Upgrade { from_hash: &'a str },
}

async fn invoke(conn: &Connection, id: &str, behavior: &dyn Behavior, effects: &dyn EffectHandler, hook: Hook<'_>) -> Result<(), Trap> {
    let runtime = |error: anyhow::Error| Trap { message: format!("actor {id} seq -1: {error:#}"), runtime: true, durability: false };
    let counter = actor::meta(conn, "hook_counter")
        .await
        .map_err(runtime)?
        .parse::<i64>()
        .map_err(|e| runtime(e.into()))?
        .checked_add(1)
        .context("hook counter overflow")
        .map_err(runtime)?;
    actor::set_meta(conn, "hook_counter", &counter.to_string()).await.map_err(runtime)?;
    let seq = counter.checked_neg().context("hook sequence overflow").map_err(runtime)?;
    let generation =
        actor::meta(conn, "generation").await.map_err(runtime)?.parse().map_err(|e| runtime(anyhow::anyhow!("invalid generation: {e}")))?;
    let mut cx =
        Ctx { conn, actor_id: id, seq, sender: None, generation, idx: 0, random_counter: 0, effects, failure: None, deferred: false };
    let mut future = Box::pin(async {
        match hook {
            Hook::Terminate { reason } => behavior.terminate(&mut cx, reason).await,
            Hook::Upgrade { from_hash } => behavior.upgrade(&mut cx, from_hash).await,
        }
    });
    let result = std::future::poll_fn(|task| match std::panic::catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(task))) {
        Ok(result) => result,
        Err(payload) => {
            let message = if let Some(message) = payload.downcast_ref::<String>() {
                message.clone()
            } else if let Some(message) = payload.downcast_ref::<&str>() {
                (*message).to_owned()
            } else {
                "lifecycle hook panicked".into()
            };
            Poll::Ready(Err(Trap::new(format!("actor {id} seq {seq}: {message}"))))
        }
    })
    .await;
    drop(future);
    Trap::finish(cx.failure.take(), result)?;
    if cx.deferred {
        return Err(Trap::new(format!("actor {id} seq {seq}: lifecycle hooks cannot defer a mailbox message")));
    }
    Ok(())
}

pub(crate) async fn terminate(
    conn: &Connection,
    id: &str,
    reason: &str,
    behavior: &dyn Behavior,
    effects: &dyn EffectHandler,
) -> Result<()> {
    if reason == "kill" {
        return Ok(());
    }
    conn.execute("SAVEPOINT terminate_hook", ()).await?;
    let result = invoke(conn, id, behavior, effects, Hook::Terminate { reason }).await;
    if let Err(error) = result {
        let seq: i64 = actor::meta(conn, "hook_counter").await?.parse::<i64>()?.checked_neg().context("hook sequence overflow")?;
        conn.execute("ROLLBACK TO terminate_hook", ()).await?;
        conn.execute("RELEASE terminate_hook", ()).await?;
        if error.runtime {
            return Err(error.into());
        }
        actor::set_meta(conn, "hook_counter", &seq.checked_neg().context("hook counter overflow")?.to_string()).await?;
        conn.execute(
            "INSERT INTO dead_letters(seq,msg,error,at) VALUES (?,?,?,?)",
            turso::params![seq, reason.as_bytes(), format!("actor {id} seq {seq}: {}", error.message), crate::effects::now()?],
        )
        .await?;
        return record_termination(conn, behavior, reason, true).await;
    }
    conn.execute("RELEASE terminate_hook", ()).await?;
    record_termination(conn, behavior, reason, false).await
}

pub(crate) async fn upgrade(
    conn: &Connection,
    id: &str,
    behavior: &dyn Behavior,
    from_hash: &str,
    effects: &dyn EffectHandler,
) -> Result<()> {
    invoke(conn, id, behavior, effects, Hook::Upgrade { from_hash }).await.map_err(Into::into)
}

pub(crate) struct Stop<'a> {
    pub reason: &'a str,
    pub key: &'a str,
    pub initiator: &'a str,
}
pub(crate) async fn stop(
    conn: &mut Connection,
    id: &str,
    operation: Stop<'_>,
    behavior: &dyn Behavior,
    effects: &dyn EffectHandler,
    node: &crate::Node,
) -> Result<()> {
    let Stop { reason, key, initiator } = operation;
    let tx = conn.transaction().await?;
    if !crate::supervision::applied(&tx, key).await? && actor::status(&tx).await? != Status::Stopped {
        terminate(&tx, id, reason, behavior, effects).await?;
    }
    let reason = if reason == "kill" { "killed" } else { reason };
    actor::stop_state(&tx, id, reason, key, initiator).await?;
    node.commit_control(id, tx).await?;
    node.close_drivers(Some(id), None).await?;
    Ok(())
}

impl crate::Node {
    pub(crate) async fn replay_terminations(
        &self,
        source: &Connection,
        conn: &mut Connection,
        epoch: i64,
        effects: &crate::effects::ReplayEffects,
    ) -> Result<Option<crate::Verdict>> {
        let rows = actor::query(source, "SELECT value FROM meta WHERE key LIKE 'termination:%'", ()).await?;
        let mut operations = Vec::new();
        for row in rows.rows {
            operations.push(serde_json::from_str::<Termination>(&row.get::<String>(0)?)?);
        }
        operations.sort_by_key(|operation| operation.counter);
        for operation in operations {
            if operation.epoch > epoch || operation.counter <= actor::meta(conn, "hook_counter").await?.parse::<i64>()? {
                continue;
            }
            let changes = actor::query(
                source,
                "SELECT behavior_hash,author,rationale FROM code_changes WHERE seq>? AND seq<=? ORDER BY seq",
                turso::params![actor::code(conn).await?.revision, operation.revision],
            )
            .await?;
            for row in changes.rows {
                let hash: String = row.get(0)?;
                if let Some(verdict) = crate::history::promote_replay(
                    conn,
                    actor::behavior(&self.registry, &hash).await?.as_ref(),
                    &row.get::<String>(1)?,
                    &row.get::<String>(2)?,
                    effects,
                )
                .await?
                {
                    return Ok(Some(verdict));
                }
            }
            ensure!(
                actor::meta(conn, "hook_counter").await?.parse::<i64>()?.checked_add(1) == Some(operation.counter),
                "termination history has a hook counter gap at {}",
                operation.counter
            );
            ensure!(
                actor::code(conn).await?.hash == operation.behavior_hash,
                "termination history behavior differs at hook {}",
                operation.counter
            );
            let tx = conn.transaction().await?;
            if operation.trapped {
                // A trapped hook rolled back its effects and domain writes. Its
                // audit row arrives with the final runtime-table overlay.
                actor::set_meta(&tx, "hook_counter", &operation.counter.to_string()).await?;
            } else {
                let seq = operation.counter.checked_neg().context("termination replay sequence overflow")?;
                let identity = actor::meta(&tx, "replay_source").await?;
                effects.begin(seq).await;
                let result = terminate(
                    &tx,
                    &identity,
                    &operation.reason,
                    actor::behavior(&self.registry, &operation.behavior_hash).await?.as_ref(),
                    effects,
                )
                .await;
                if let Some(verdict) = effects.finish(seq, result.is_ok()).await? {
                    tx.rollback().await?;
                    return Ok(Some(verdict));
                }
                result?;
                let key = format!("termination:{}", operation.counter);
                let recorded: Termination = serde_json::from_str(&actor::meta(&tx, &key).await?)?;
                ensure!(!recorded.trapped, "termination replay trapped at hook {}", operation.counter);
            }
            tx.commit().await?;
        }
        Ok(None)
    }
}
