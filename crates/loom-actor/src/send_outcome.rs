use crate::{ActorId, Node, actor};
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SendOutcome {
    Complete { id: ActorId, seq: i64, cursor: i64 },
    Failed { id: ActorId, seq: i64, cursor: i64, cause: String },
    Pending { id: ActorId, seq: i64, cursor: i64, cause: String },
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct MessageIdentity {
    pub id: ActorId,
    pub generation: i64,
    pub seq: i64,
}

pub(crate) type Outcomes = Arc<Mutex<HashMap<MessageIdentity, SendOutcome>>>;

struct Watch {
    identity: MessageIdentity,
    outcomes: Outcomes,
}
impl Drop for Watch {
    fn drop(&mut self) {
        if let Ok(mut outcomes) = self.outcomes.lock() {
            outcomes.remove(&self.identity);
        }
    }
}

impl Node {
    /// Drain the normal scheduler while preserving this message's outcome across supervisor resets.
    pub async fn send_with_outcome(&self, id: &str, key: &str, msg: &[u8]) -> Result<SendOutcome> {
        let _admission = self.admit().await?;
        let _run = self.run_gate.lock().await;
        let actor = self.open_actor(id).await?;
        let mut conn = actor.conn.lock().await;
        let tx = conn.transaction().await?;
        actor::inject(&tx, key, "external", msg).await?;
        let rows = actor::query(&tx, "SELECT seq,state FROM inbox WHERE key=?", [key]).await?;
        let row = rows.rows.first().context("sent actor message missing from inbox")?;
        let seq: i64 = row.get(0)?;
        let state: String = row.get(1)?;
        let identity = MessageIdentity { id: id.into(), generation: actor::meta(&tx, "generation").await?.parse()?, seq };
        let cursor = actor::cursor(&tx).await?;
        let failures = actor::query(&tx, "SELECT error FROM dead_letters WHERE seq=?", [seq]).await?;
        let initial = if let Some(failure) = failures.rows.first() {
            SendOutcome::Failed { id: id.into(), seq, cursor, cause: failure.get(0)? }
        } else if state == "done" {
            SendOutcome::Complete { id: id.into(), seq, cursor }
        } else {
            SendOutcome::Pending { id: id.into(), seq, cursor, cause: format!("message is {state}") }
        };
        self.commit_control(id, tx).await?;
        self.send_outcomes.lock().map_err(|_| anyhow!("actor send outcomes poisoned"))?.insert(identity.clone(), initial);
        let watch = Watch { identity, outcomes: self.send_outcomes.clone() };
        drop(conn);
        self.wake_actor(id)?;
        let drained = self.run_until_idle_inner().await;
        let mut outcome = self
            .send_outcomes
            .lock()
            .map_err(|_| anyhow!("actor send outcomes poisoned"))?
            .get(&watch.identity)
            .cloned()
            .context("actor send outcome missing")?;
        // A recorded guest failure remains the primary result even if subsequent supervision fails.
        if let SendOutcome::Failed { cause, .. } = &mut outcome {
            if let Err(error) = drained {
                cause.push_str(&format!("; subsequent actor scheduling failed: {error:#}"));
            }
            return Ok(outcome);
        }
        drained?;
        if matches!(outcome, SendOutcome::Pending { .. }) {
            let actor = self.open_actor(id).await?;
            let conn = actor.conn.lock().await;
            let generation: i64 = actor::meta(&conn, "generation").await?.parse()?;
            let cursor = actor::cursor(&conn).await?;
            outcome = if generation != watch.identity.generation {
                SendOutcome::Failed {
                    id: id.into(),
                    seq,
                    cursor,
                    cause: format!("actor {id} seq {seq}: incarnation reset before this message completed"),
                }
            } else {
                let status = actor::meta(&conn, "status").await?;
                SendOutcome::Pending {
                    id: id.into(),
                    seq,
                    cursor,
                    cause: format!("actor {id} seq {seq}: message not completed; actor is {status}"),
                }
            };
        }
        Ok(outcome)
    }

    pub(crate) fn record_send_outcome(&self, identity: MessageIdentity, outcome: SendOutcome) -> Result<()> {
        let mut outcomes = self.send_outcomes.lock().map_err(|_| anyhow!("actor send outcomes poisoned"))?;
        if let Some(current) = outcomes.get_mut(&identity)
            && matches!(current, SendOutcome::Pending { .. })
        {
            *current = outcome;
        }
        Ok(())
    }
}
