//! Lifecycle hooks share the actor transaction and use a separate effect sequence.
use crate::{Behavior, Ctx, EffectHandler, Status, Trap, actor};
use anyhow::{Context, Result};
use std::{future::Future, panic::AssertUnwindSafe, task::Poll};
use turso::Connection;

enum Hook<'a> {
    Terminate { reason: &'a str },
    Upgrade { from_hash: &'a str },
}

async fn invoke(conn: &Connection, id: &str, behavior: &dyn Behavior, effects: &dyn EffectHandler, hook: Hook<'_>) -> Result<(), Trap> {
    let runtime = |error: anyhow::Error| Trap { message: format!("actor {id} seq -1: {error:#}"), runtime: true };
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
        return Ok(());
    }
    conn.execute("RELEASE terminate_hook", ()).await?;
    Ok(())
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

pub(crate) async fn stop(
    conn: &mut Connection,
    id: &str,
    reason: &str,
    key: &str,
    initiator: &str,
    behavior: &dyn Behavior,
    effects: &dyn EffectHandler,
) -> Result<()> {
    let tx = conn.transaction().await?;
    if !crate::supervision::applied(&tx, key).await? && actor::status(&tx).await? != Status::Stopped {
        terminate(&tx, id, reason, behavior, effects).await?;
    }
    let reason = if reason == "kill" { "killed" } else { reason };
    actor::stop_state(&tx, id, reason, key, initiator).await?;
    tx.commit().await?;
    Ok(())
}
