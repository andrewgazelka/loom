// Scoped here until lib.rs exposes the public memo module.
#[path = "memo.rs"]
pub mod memo;
#[cfg(test)]
#[path = "../tests/memo.rs"]
mod memo_tests;

use crate::{Actor, ActorId, Node, TableDifference, TableHash, Verdict, actor, effects::ReplayEffects, ids};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use turso::{Connection, Value};

/// Removes tracked paths unless `disarm` ran; guards fork staging files on every early return.
struct Cleanup {
    paths: Vec<PathBuf>,
}
impl Cleanup {
    fn track(&mut self, path: PathBuf) {
        self.paths.push(path);
    }
    fn disarm(&mut self) {
        self.paths.clear();
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        for path in self.paths.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Node {
    async fn fork_from(&self, source: &Connection, id: &str, at: i64) -> Result<Actor> {
        ensure!(self.config.io != crate::Io::Memory, "actor {id} seq {at}: historical snapshots require persistent I/O");
        ensure!(at >= 0 && at <= actor::cursor(source).await?, "actor {id} seq {at}: fork sequence outside history");
        let target_epoch = boundary(source, id, at).await?;
        let rows = actor::query(source, "SELECT seq,path FROM snapshots WHERE seq<=? ORDER BY seq DESC LIMIT 1", [at]).await?;
        let snapshot = rows.rows.first().context("no snapshot at requested sequence")?;
        let snapshot_seq: i64 = snapshot.get(0)?;
        let path: String = snapshot.get(1)?;
        let fork_id = ids::root();
        let staging = self.path(&fork_id).with_extension("forking");
        std::fs::copy(&path, &staging)?;
        let mut cleanup = Cleanup { paths: Vec::new() };
        cleanup.track(staging.clone());
        cleanup.track(PathBuf::from(format!("{}-wal", staging.display())));
        let mut conn = actor::connect(&staging, self.config.io).await?;
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
        actor::set_meta(&tx, "node_root", "false").await?;
        tx.execute("INSERT OR IGNORE INTO snapshots(seq,path) VALUES (?,?)", turso::params![snapshot_seq, path]).await?;
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
        if let Some(verdict) = self.replay(source, &mut conn, id, at, &replay, true).await? {
            anyhow::bail!("historical replay failed: {verdict:?}");
        }
        let audit = actor::query(source, "SELECT seq,msg,error,at FROM dead_letters WHERE seq<=? ORDER BY seq", [at]).await?;
        for row in audit.rows {
            let values = (0..row.column_count()).map(|i| row.get_value(i)).collect::<turso::Result<Vec<_>>>()?;
            conn.execute("INSERT OR IGNORE INTO dead_letters(seq,msg,error,at) VALUES (?,?,?,?)", values).await?;
        }
        // Publish the replayed file through a complete, WAL-independent copy.
        let ready = self.path(&fork_id).with_extension("ready");
        cleanup.track(ready.clone());
        conn.execute(format!("VACUUM INTO '{}'", ready.to_str().context("non-UTF8 fork path")?.replace('\'', "''")), ()).await?;
        std::fs::rename(&ready, self.path(&fork_id))?;
        drop(conn);
        cleanup.disarm();
        std::fs::remove_file(&staging)?;
        let wal = format!("{}-wal", staging.display());
        if Path::new(&wal).exists() {
            std::fs::remove_file(wal)?;
        }
        self.open(&fork_id).await
    }

    async fn replay(
        &self,
        source: &Connection,
        conn: &mut Connection,
        id: &str,
        at: i64,
        effects: &ReplayEffects,
        historical: bool,
    ) -> Result<Option<Verdict>> {
        let identity = actor::meta(conn, "replay_source").await?;
        let target_epoch = boundary(source, id, at).await?;
        let start_epoch: i64 = actor::meta(conn, "commit_epoch").await?.parse()?;
        ensure!(start_epoch <= target_epoch, "actor {id} seq {at}: snapshot lies after replay boundary");
        for epoch in (start_epoch + 1)..=target_epoch {
            let seq: i64 = actor::meta(source, &format!("commit_order:{epoch}")).await?.parse()?;
            ensure!(seq <= at, "actor {id} seq {seq}: replay crosses requested boundary {at}");
            let rows = actor::query(conn, "SELECT msg,sender,state FROM inbox WHERE seq=?", [seq]).await?;
            let row = rows.rows.first().with_context(|| format!("actor {id} seq {seq}: history inbox has a gap"))?;
            ensure!(row.get::<String>(2)? != "done", "actor {id} seq {seq}: history commits a message twice");
            let sender: String = row.get(1)?;
            let message = actor::Message { seq, msg: row.get(0)?, sender: if sender.starts_with("a0") { Some(sender) } else { None } };
            if historical {
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
                    if let Some(verdict) = promote_replay(
                        conn,
                        actor::behavior(&self.registry, &hash)?.as_ref(),
                        &row.get::<String>(1)?,
                        &row.get::<String>(2)?,
                        effects,
                    )
                    .await?
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
            let behavior = actor::behavior(&self.registry, &code.hash)?;
            for retry in 0..=self.config.max_retries {
                effects.begin(message.seq).await;
                let result = actor::attempt(conn, &identity, &message, behavior.as_ref(), code.revision, effects, None).await;
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
        ensure!(actor::cursor(conn).await? == at, "actor {id} seq {at}: replay did not reach its contiguous boundary");
        Ok(None)
    }

    pub(crate) async fn fork_inner(&self, id: &str, at: i64) -> Result<ActorId> {
        let actor = self.open(id).await?;
        let source = actor.conn.lock().await;
        self.fork_from(&source, id, at).await.map(|actor| actor.id).with_context(|| format!("actor {id} seq {at}: fork"))
    }

    pub(crate) async fn validate_inner(&self, id: &str, candidate: &str, k: i64) -> Result<Verdict> {
        Ok(self.validate_assertions(id, candidate, k, &[]).await?.verdict)
    }

    pub async fn validate_assertions(&self, id: &str, candidate: &str, k: i64, assertions: &[String]) -> Result<crate::ValidationResult> {
        Ok(self.validation_record(id, candidate, k, assertions, memo::MemoConfig::default()).await?.result)
    }

    pub async fn validate_with_memo_config(
        &self,
        id: &str,
        candidate: &str,
        k: i64,
        assertions: &[String],
        config: memo::MemoConfig,
    ) -> Result<crate::ValidationResult> {
        Ok(self.validation_record(id, candidate, k, assertions, config).await?.result)
    }

    async fn validation_record(
        &self,
        id: &str,
        candidate: &str,
        k: i64,
        assertions: &[String],
        config: memo::MemoConfig,
    ) -> Result<memo::Record> {
        ensure!(self.config.io != crate::Io::Memory, "actor {id}: historical snapshots require persistent I/O");
        i64::try_from(config.max_rows).context("validation_memo max_rows exceeds SQLite integer")?;
        let actor = self.open(id).await?;
        let source = actor.conn.lock().await;
        let cursor = actor::cursor(&source).await?;
        ensure!(k >= 0 && k <= cursor, "actor {id} seq {cursor}: validation window outside history");
        let behavior = actor::behavior(&self.registry, candidate)?;
        let key = memo::key(&source, candidate, cursor - k, cursor, assertions).await?;
        if let Some(record) = memo::lookup(self, &key).await? {
            memo::store(self, &key, &record, config).await?;
            return Ok(record);
        }
        let fork = self.fork_from(&source, id, cursor - k).await?;
        let mut conn = fork.conn.lock().await;
        let effects = ReplayEffects::load(&source).await?;
        if let Some(verdict) = promote_replay(&mut conn, behavior.as_ref(), "validation", "candidate", &effects).await? {
            let record = memo::Record {
                key: key.clone(),
                result: crate::ValidationResult { verdict, assertions: Vec::new() },
                tables: table_hashes(&conn).await?,
                outbox_hash: memo::outbox_hash(&conn, cursor - k, cursor).await?,
            };
            memo::store(self, &key, &record, config).await?;
            return Ok(record);
        }
        if let Some(verdict) = self.replay(&source, &mut conn, id, cursor, &effects, false).await? {
            let record = memo::Record {
                key: key.clone(),
                result: crate::ValidationResult { verdict, assertions: Vec::new() },
                tables: table_hashes(&conn).await?,
                outbox_hash: memo::outbox_hash(&conn, cursor - k, cursor).await?,
            };
            memo::store(self, &key, &record, config).await?;
            return Ok(record);
        }
        let original = table_hashes(&source).await?;
        let replayed = table_hashes(&conn).await?;
        let mut differences = Vec::new();
        for name in original.keys().chain(replayed.keys()).collect::<BTreeSet<_>>() {
            if original.get(name) != replayed.get(name) {
                differences.push(TableDifference {
                    name: name.clone(),
                    original_hash: original.get(name).cloned().unwrap_or_else(|| "absent".into()),
                    fork_hash: replayed.get(name).cloned().unwrap_or_else(|| "absent".into()),
                });
            }
        }
        let verdict = if differences.is_empty() {
            Verdict::Matched { tables: original.into_iter().map(|(name, hash)| TableHash { name, hash }).collect() }
        } else {
            Verdict::Differs { tables: differences }
        };
        let mut results = Vec::new();
        for query in assertions {
            let rows = actor::inspect_query(&conn, query, Vec::new())
                .await
                .with_context(|| format!("actor {id} seq {cursor}: validation assertion"))?;
            let passed = rows.rows.len() == 1
                && rows.columns.len() == 1
                && match rows.rows[0].get_value(0)? {
                    Value::Integer(value) => value != 0,
                    Value::Real(value) => value != 0.0,
                    _ => false,
                };
            results.push(crate::AssertionResult { query: query.clone(), passed });
        }
        let record = memo::Record {
            key: key.clone(),
            result: crate::ValidationResult { verdict, assertions: results },
            tables: replayed,
            outbox_hash: memo::outbox_hash(&conn, cursor - k, cursor).await?,
        };
        memo::store(self, &key, &record, config).await?;
        Ok(record)
    }

    /// Validate and promote under the actor lock; the returned cutoff compares historical sends.
    pub async fn promote_report(&self, id: &str, hash: &str, k: i64) -> Result<memo::PromoteReport> {
        let record = self.validation_record(id, hash, k, &[], memo::MemoConfig::default()).await?;
        let actor = self.open(id).await?;
        let mut conn = actor.conn.lock().await;
        let end = actor::cursor(&conn).await?;
        // Validation released the lock. Refuse a moving history instead of reporting a stale cutoff.
        let key = memo::key(&conn, hash, end - k, end, &[]).await?;
        ensure!(key == record.key, "promote_report: history changed after validation; retry");
        let report = self.promotion_cutoff(&conn, record, end - k, end).await?;
        actor::promote(
            &mut conn,
            self.behavior(hash)?.as_ref(),
            "promote_report",
            "validated candidate",
            &crate::effects::RuntimeEffects { node: self, external: self.effects.as_ref() },
        )
        .await?;
        drop(conn);
        self.sync_index(id).await?;
        Ok(report)
    }

    async fn promotion_cutoff(&self, source: &Connection, record: memo::Record, at: i64, end: i64) -> Result<memo::PromoteReport> {
        let rows =
            actor::query(source, "SELECT DISTINCT target FROM outbox WHERE seq>? AND seq<=? ORDER BY target", turso::params![at, end])
                .await?;
        let mut receivers = Vec::new();
        for row in rows.rows {
            receivers.push(row.get(0)?);
        }
        Ok(memo::PromoteReport {
            downstream_unaffected: record.outbox_hash == memo::outbox_hash(source, at, end).await?,
            verdict: record.result.verdict,
            receivers,
        })
    }
}

async fn promote_replay(
    conn: &mut Connection,
    behavior: &dyn crate::Behavior,
    author: &str,
    rationale: &str,
    effects: &ReplayEffects,
) -> Result<Option<Verdict>> {
    let counter: i64 = actor::meta(conn, "hook_counter").await?.parse()?;
    let seq = counter.checked_add(1).and_then(i64::checked_neg).context("hook sequence overflow")?;
    effects.begin(seq).await;
    let result = actor::promote(conn, behavior, author, rationale, effects).await;
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
