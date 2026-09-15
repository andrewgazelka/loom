#[path = "memo.rs"]
pub mod memo;
#[cfg(test)]
#[path = "../tests/memo.rs"]
mod memo_tests;
mod memory;
mod validation;
pub(crate) use memory::MemorySnapshot;

use crate::{Actor, ActorId, Node, TableDifference, TableHash, Verdict, actor, effects::ReplayEffects, ids};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use turso::{Connection, Value};

/// Historical replay preserves recorded code; candidates replace it; remote replay includes controls.
pub(crate) enum ReplayMode {
    Historical,
    Candidate,
    Remote,
}

impl Node {
    async fn fork_from(&self, source: &Connection, id: &str, at: i64) -> Result<Actor> {
        ensure!(at >= 0 && at <= actor::cursor(source).await?, "actor {id} seq {at}: fork sequence outside history");
        let target_epoch = boundary(source, id, at).await?;
        let rows = actor::query(source, "SELECT seq,path FROM snapshots WHERE seq<=? ORDER BY seq DESC LIMIT 1", [at]).await?;
        let snapshot = rows.rows.first().context("no snapshot at requested sequence")?;
        let snapshot_seq: i64 = snapshot.get(0)?;
        let path: String = snapshot.get(1)?;
        let fork_id = ids::root();
        let mut conn = self.snapshot_connection(&path).await?;
        crate::capability::migrate(&conn).await?;
        self.migrate_authority(&conn).await?;
        let snapshot_cursor = actor::cursor(&conn).await?;
        let snapshot_epoch: i64 = actor::meta(&conn, "commit_epoch").await?.parse()?;
        ensure!(
            snapshot_cursor == snapshot_seq && snapshot_epoch <= target_epoch,
            "actor {id} seq {at}: snapshot does not represent the requested historical boundary"
        );
        ensure!(
            boundary(&conn, id, snapshot_seq).await? == snapshot_epoch,
            "actor {id} seq {at}: snapshot contains noncontiguous completed messages"
        );
        let tx = conn.transaction().await?;
        actor::set_meta(&tx, "id", &fork_id).await?;
        actor::set_meta(&tx, "parent", id).await?;
        actor::set_meta(&tx, "status", "fork").await?;
        actor::set_meta(&tx, "durability", "ephemeral").await?;
        actor::set_meta(&tx, "node_root", "false").await?;
        tx.execute(
            "INSERT INTO snapshots(seq,path) VALUES (?,?) ON CONFLICT(seq) DO UPDATE SET path=excluded.path",
            turso::params![snapshot_seq, path],
        )
        .await?;
        let identity = actor::query(source, "SELECT value FROM meta WHERE key='replay_source'", ()).await?;
        let identity = match identity.rows.first() {
            Some(row) => row.get::<String>(0)?,
            None => id.to_owned(),
        };
        actor::set_meta(&tx, "replay_source", &identity).await?;
        let inbox = actor::query(source, "SELECT seq,key,sender,msg,received_at FROM inbox ORDER BY seq", ()).await?;
        for row in inbox.rows {
            let values = (0..row.column_count()).map(|i| row.get_value(i)).collect::<turso::Result<Vec<_>>>()?;
            tx.execute("INSERT OR IGNORE INTO inbox(seq,key,sender,msg,received_at) VALUES (?,?,?,?,?)", values).await?;
        }
        tx.commit().await?;
        let replay = ReplayEffects::load(source).await?;
        if let Some(verdict) = self.replay(source, &mut conn, id, at, &replay, ReplayMode::Historical).await? {
            anyhow::bail!("historical replay failed: {verdict:?}");
        }
        let audit = actor::query(source, "SELECT seq,msg,error,at FROM dead_letters WHERE seq<=? ORDER BY seq", [at]).await?;
        for row in audit.rows {
            let values = (0..row.column_count()).map(|i| row.get_value(i)).collect::<turso::Result<Vec<_>>>()?;
            conn.execute("INSERT OR IGNORE INTO dead_letters(seq,msg,error,at) VALUES (?,?,?,?)", values).await?;
        }
        let conn = std::sync::Arc::new(tokio::sync::Mutex::new(conn));
        Ok(Actor { id: fork_id, conn, managed: self.remote.is_some(), node: self.clone() })
    }

    pub(crate) async fn replay(
        &self,
        source: &Connection,
        conn: &mut Connection,
        id: &str,
        at: i64,
        effects: &ReplayEffects,
        mode: ReplayMode,
    ) -> Result<Option<Verdict>> {
        let identity = actor::meta(conn, "replay_source").await?;
        let target_epoch = if matches!(mode, ReplayMode::Remote) {
            actor::meta(source, "commit_epoch").await?.parse()?
        } else {
            boundary(source, id, at).await?
        };
        let start_epoch: i64 = actor::meta(conn, "commit_epoch").await?.parse()?;
        ensure!(start_epoch <= target_epoch, "actor {id} seq {at}: snapshot lies after replay boundary");
        for epoch in (start_epoch + 1)..=target_epoch {
            if matches!(mode, ReplayMode::Remote) {
                crate::supervisor_store::replay_host_spawns(source, conn, epoch - 1).await?;
            }
            if let Some(verdict) = self.replay_terminations(source, conn, epoch - 1, effects, matches!(mode, ReplayMode::Candidate)).await?
            {
                return Ok(Some(verdict));
            }
            let seq: i64 = actor::meta(source, &format!("commit_order:{epoch}")).await?.parse()?;
            ensure!(matches!(mode, ReplayMode::Remote) || seq <= at, "actor {id} seq {seq}: replay crosses requested boundary {at}");
            let rows = actor::query(conn, "SELECT msg,sender,state FROM inbox WHERE seq=?", [seq]).await?;
            let row = rows.rows.first().with_context(|| format!("actor {id} seq {seq}: history inbox has a gap"))?;
            ensure!(row.get::<String>(2)? != "done", "actor {id} seq {seq}: history commits a message twice");
            let sender: String = row.get(1)?;
            let message = actor::Message {
                seq,
                msg: row.get(0)?,
                sender: if sender.starts_with("a0") || sender.starts_with("drv:") { Some(sender) } else { None },
            };
            if !matches!(mode, ReplayMode::Candidate) {
                let skipped = actor::query(source, "SELECT value FROM meta WHERE key=?", [format!("skipped:{}", message.seq)]).await?;
                let revision: i64 = actor::meta(source, &format!("code_at:{}", message.seq)).await?.parse()?;
                let changes = actor::query(
                    source,
                    "SELECT behavior_hash,author,rationale FROM code_changes WHERE seq>? AND seq<=? ORDER BY seq",
                    turso::params![actor::code(conn).await?.revision, revision],
                )
                .await?;
                for row in changes.rows {
                    let hash: String = row.get(0)?;
                    let behavior = crate::view::behavior_on(&self.registry, conn, &hash).await?;
                    if let Some(verdict) =
                        promote_replay(conn, behavior.as_ref(), &row.get::<String>(1)?, &row.get::<String>(2)?, effects).await?
                    {
                        return Ok(Some(verdict));
                    }
                }
                if !skipped.rows.is_empty() {
                    let tx = conn.transaction().await?;
                    crate::mailbox::complete(&tx, message.seq).await?;
                    actor::set_meta(&tx, &format!("skipped:{}", message.seq), "1").await?;
                    actor::set_meta(&tx, &format!("code_at:{}", message.seq), &revision.to_string()).await?;
                    tx.commit().await?;
                    continue;
                }
            }
            let code = actor::code(conn).await?;
            let behavior = crate::view::behavior_on(&self.registry, conn, &code.hash).await?;
            for retry in 0..=self.config.max_retries {
                effects.begin(message.seq).await;
                let state = actor::AttemptState {
                    generation: actor::meta(conn, "generation").await?.parse()?,
                    revision: code.revision,
                    epoch: actor::meta(conn, "commit_epoch").await?.parse()?,
                };
                let result = actor::attempt(conn, &identity, &message, behavior.as_ref(), &state, effects, None).await;
                let completed = actor::meta(conn, "commit_epoch").await?.parse::<i64>()? == epoch;
                if let Some(verdict) = effects.finish(message.seq, result.is_ok() && completed).await? {
                    return Ok(Some(verdict));
                }
                match result {
                    Ok(_) if completed => break,
                    Ok(_) => {
                        return Ok(Some(Verdict::Trapped {
                            seq: message.seq,
                            error: format!("actor {id} seq {}: replay deferred a historically committed message", message.seq),
                        }));
                    }
                    Err(error) if error.runtime && retry < self.config.max_retries => {
                        tokio::time::sleep(self.config.retry_backoff.saturating_mul(u32::try_from(retry + 1)?)).await;
                    }
                    Err(error) if error.runtime => {
                        anyhow::bail!("actor {id} seq {}: replay runtime failure: {}", message.seq, error.message)
                    }
                    Err(error) => {
                        return Ok(Some(Verdict::Trapped { seq: message.seq, error: error.message }));
                    }
                }
            }
        }
        if matches!(mode, ReplayMode::Remote) {
            crate::supervisor_store::replay_host_spawns(source, conn, target_epoch).await?;
        }
        if let Some(verdict) = self.replay_terminations(source, conn, target_epoch, effects, matches!(mode, ReplayMode::Candidate)).await? {
            return Ok(Some(verdict));
        }
        if matches!(mode, ReplayMode::Remote) {
            let revision = actor::code(source).await?.revision;
            let changes = actor::query(
                source,
                "SELECT behavior_hash,author,rationale FROM code_changes WHERE seq>? AND seq<=? ORDER BY seq",
                turso::params![actor::code(conn).await?.revision, revision],
            )
            .await?;
            for row in changes.rows {
                let hash: String = row.get(0)?;
                let behavior = crate::view::behavior_on(&self.registry, conn, &hash).await?;
                if let Some(verdict) =
                    promote_replay(conn, behavior.as_ref(), &row.get::<String>(1)?, &row.get::<String>(2)?, effects).await?
                {
                    return Ok(Some(verdict));
                }
            }
        }
        ensure!(actor::cursor(conn).await? == at, "actor {id} seq {at}: replay did not reach its contiguous boundary");
        Ok(None)
    }

    pub(crate) async fn fork_inner(&self, id: &str, at: i64) -> Result<ActorId> {
        let actor = self.open_actor(id).await?;
        let source = actor.conn.lock().await;
        let fork = self.fork_from(&source, id, at).await.with_context(|| format!("actor {id} seq {at}: fork"))?;
        self.memory_ids.lock().map_err(|_| anyhow::anyhow!("memory actor registry poisoned"))?.push(fork.id.clone());
        self.connections.lock().await.insert(fork.id.clone(), fork.conn);
        Ok(fork.id)
    }
}

pub(crate) async fn promote_candidate(
    conn: &mut Connection,
    behavior: &dyn crate::Behavior,
    effects: &ReplayEffects,
) -> Result<Option<Verdict>> {
    effects.begin(i64::MIN).await;
    let result = actor::promote_candidate(conn, behavior, effects).await;
    if let Some(verdict) = effects.finish(i64::MIN, result.is_ok()).await? {
        return Ok(Some(verdict));
    }
    match result {
        Ok(()) => Ok(None),
        Err(error) if error.downcast_ref::<crate::Trap>().is_some_and(|trap| !trap.runtime) => {
            Ok(Some(Verdict::Trapped { seq: i64::MIN, error: format!("candidate upgrade: {error:#}") }))
        }
        Err(error) => Err(error),
    }
}

pub(crate) async fn promote_replay(
    conn: &mut Connection,
    behavior: &dyn crate::Behavior,
    author: &str,
    rationale: &str,
    effects: &ReplayEffects,
) -> Result<Option<Verdict>> {
    let counter: i64 = actor::meta(conn, "hook_counter").await?.parse()?;
    let seq = counter.checked_add(1).and_then(i64::checked_neg).context("hook sequence overflow")?;
    effects.begin(seq).await;
    let result = actor::promote(conn, behavior, author, rationale, effects, None).await;
    if let Some(verdict) = effects.finish(seq, result.is_ok()).await? {
        return Ok(Some(verdict));
    }
    if let Err(error) = result {
        if error.downcast_ref::<crate::Trap>().is_some_and(|trap| !trap.runtime) {
            return Ok(Some(Verdict::Trapped {
                seq,
                error: format!("actor {} seq {seq}: upgrade: {error:#}", actor::meta(conn, "id").await?),
            }));
        }
        return Err(error);
    }
    Ok(None)
}

async fn boundary(conn: &Connection, id: &str, at: i64) -> Result<i64> {
    actor::meta(conn, &format!("boundary:{at}"))
        .await
        .with_context(|| format!("actor {id} seq {at}: inaccessible noncontiguous historical cut"))?
        .parse()
        .with_context(|| format!("actor {id} seq {at}: invalid historical boundary"))
}

async fn table_hashes(conn: &Connection) -> Result<BTreeMap<String, String>> {
    let tables = actor::query(conn, "SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name", ()).await?;
    let mut result = BTreeMap::new();
    for row in tables.rows {
        let name: String = row.get(0)?;
        if crate::schema::SYSTEM_TABLES.contains(&name.as_str()) || name.starts_with("sqlite_") {
            continue;
        }
        let hash = memo::rows_hash(conn, &format!("SELECT * FROM \"{}\" ORDER BY rowid", name.replace('"', "\"\"")), ()).await?;
        result.insert(name, hash);
    }
    Ok(result)
}
