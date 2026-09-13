use super::*;
use std::{future::Future, panic::AssertUnwindSafe, task::Poll};

/// Poll-by-poll panic boundary includes the async-trait method's initial call.
async fn handle(behavior: &dyn Behavior, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
    let mut future = Box::pin(async { behavior.handle(cx, msg).await });
    std::future::poll_fn(|task| match std::panic::catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(task))) {
        Ok(result) => result,
        Err(payload) => {
            let message = if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_owned()
            } else {
                "handler panicked with non-string payload".into()
            };
            Poll::Ready(Err(Trap::new(message)))
        }
    })
    .await
}

/// Valid only under the connection lock. Releasing that lock invalidates all
/// fields: initialize/reset write generation, initialize/promote write code,
/// and complete/skip/poison/replay write epoch. Host Actor::sql may write any.
/// Admission status/ready are checked anew each batch; lifecycle stop/restart,
/// poison, promotion, replay/archive and pump publication cannot race this lock.
pub(crate) struct AttemptState {
    pub generation: i64,
    pub revision: i64,
    pub epoch: i64,
}

pub(crate) enum Attempt {
    Cancelled,
    Deferred,
    Complete { completion: crate::mailbox::Completion },
}

pub(crate) async fn attempt(
    conn: &mut Connection,
    id: &str,
    message: &Message,
    behavior: &dyn Behavior,
    state: &AttemptState,
    effects: &dyn EffectHandler,
    control: Option<&crate::durability::AttemptControl<'_>>,
) -> Result<Attempt, Trap> {
    let runtime = |e: anyhow::Error| Trap { message: format!("actor {id} seq {}: {e:#}", message.seq), runtime: true, durability: false };
    let tx = conn.transaction().await.map_err(|e| runtime(e.into()))?;
    let mut cx = Ctx {
        conn: &tx,
        actor_id: id,
        seq: message.seq,
        idx: 0,
        random_counter: 0,
        effects,
        failure: None,
        deferred: false,
        sender: message.sender.clone(),
        generation: state.generation,
    };
    // Cancellation is confined to the handler. Transaction completion must be
    // driven to completion even when a kill arrives during asynchronous I/O.
    let result = tokio::select! {
        result = handle(behavior, &mut cx, &message.msg) => Some(result),
        _ = async { match control.map(|c| c.cancellation) { Some(signal) => signal.notified().await, None => std::future::pending().await } } => None,
    };
    let Some(result) = result else {
        drop(cx);
        tx.rollback().await.map_err(|error| runtime(error.into()))?;
        return Ok(Attempt::Cancelled);
    };
    let result = Trap::finish(cx.failure.take(), result);
    let deferred = cx.deferred;
    drop(cx);
    if let Err(mut error) = result {
        tx.rollback().await.map_err(|e| runtime(e.into()))?;
        error.message = format!("actor {id} seq {}: {}", message.seq, error.message);
        return Err(error);
    }
    if deferred {
        tx.rollback().await.map_err(|e| runtime(e.into()))?;
        return crate::mailbox::defer(conn, id, message.seq, control.map(|c| c.node)).await.map(|()| Attempt::Deferred);
    }
    let completion = crate::mailbox::complete_at(&tx, message.seq, state.epoch, Some(state.revision)).await.map_err(runtime)?;
    if let Some(control) = control
        && let Err(error) = control.node.prepare_commit(id, &tx).await
    {
        tx.rollback().await.map_err(|e| runtime(e.into()))?;
        return Err(Trap { message: format!("actor {id}: {error:#}"), runtime: true, durability: true });
    }
    tx.commit().await.map_err(|e| {
        let mut error = runtime(e.into());
        error.durability = control.is_some_and(|c| c.node.remote.is_some());
        error
    })?;
    Ok(Attempt::Complete { completion })
}
