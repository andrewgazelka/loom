use crate::{ActorId, Node, Status, actor};
use anyhow::{Context, Result, ensure};
use std::path::Path;
use turso::Connection;

#[derive(Clone, Debug, serde::Serialize)]
pub struct MonitorInfo {
    pub reference: String,
    pub target: ActorId,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ActorInfo {
    pub status: Status,
    pub reason: String,
    pub cursor: i64,
    pub inbox_len: i64,
    pub deferred_len: i64,
    pub behavior_hash: String,
    pub links: Vec<ActorId>,
    pub monitors: Vec<MonitorInfo>,
    pub parent: Option<ActorId>,
    pub children: Vec<ActorId>,
}

impl Node {
    /// Names are unique: registering an occupied name fails rather than replacing it.
    pub async fn register(&self, name: &str, id: &str) -> Result<()> {
        async {
            ensure!(!name.is_empty(), "empty registered name");
            let actor = self.open_actor(id).await?;
            let conn = actor.conn.lock().await;
            ensure_live(&conn).await?;
            let mut slot = self.names.lock().await;
            let index = connection(&mut slot, &self.dir, self.config.io).await?;
            index.execute("INSERT INTO names(name,id) VALUES (?,?)", [name, id]).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await
        .with_context(|| format!("actor {id} seq -1: register {name}"))
    }

    pub async fn unregister(&self, name: &str) -> Result<()> {
        async {
            let mut slot = self.names.lock().await;
            let index = connection(&mut slot, &self.dir, self.config.io).await?;
            index.execute("DELETE FROM names WHERE name=?", [name]).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await
        .with_context(|| format!("actor <node> seq -1: unregister {name}"))
    }

    pub async fn whereis(&self, name: &str) -> Result<Option<ActorId>> {
        async {
            let mut slot = self.names.lock().await;
            let index = connection(&mut slot, &self.dir, self.config.io).await?;
            let rows = actor::query(index, "SELECT id FROM names WHERE name=?", [name]).await?;
            rows.rows.first().map(|row| row.get::<String>(0).map_err(anyhow::Error::from)).transpose()
        }
        .await
        .with_context(|| format!("actor <node> seq -1: whereis {name}"))
    }

    pub async fn join(&self, group: &str, id: &str) -> Result<()> {
        async {
            ensure!(!group.is_empty(), "empty group name");
            let actor = self.open_actor(id).await?;
            let conn = actor.conn.lock().await;
            ensure_live(&conn).await?;
            let mut slot = self.names.lock().await;
            let index = connection(&mut slot, &self.dir, self.config.io).await?;
            index.execute("INSERT OR IGNORE INTO groups(\"group\",id) VALUES (?,?)", [group, id]).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await
        .with_context(|| format!("actor {id} seq -1: join {group}"))
    }

    pub async fn leave(&self, group: &str, id: &str) -> Result<()> {
        async {
            let mut slot = self.names.lock().await;
            let index = connection(&mut slot, &self.dir, self.config.io).await?;
            index.execute("DELETE FROM groups WHERE \"group\"=? AND id=?", [group, id]).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await
        .with_context(|| format!("actor {id} seq -1: leave {group}"))
    }

    pub async fn members(&self, group: &str) -> Result<Vec<ActorId>> {
        async {
            let mut slot = self.names.lock().await;
            let index = connection(&mut slot, &self.dir, self.config.io).await?;
            let rows = actor::query(index, "SELECT id FROM groups WHERE \"group\"=? ORDER BY id", [group]).await?;
            rows.rows.into_iter().map(|row| row.get::<String>(0).map_err(anyhow::Error::from)).collect::<Result<Vec<_>>>()
        }
        .await
        .with_context(|| format!("actor <node> seq -1: members {group}"))
    }

    /// Each actor promotion commits independently; failure reports the actor and stops the sweep.
    pub async fn promote_where(&self, old_hash: &str, new_hash: &str, author: &str, rationale: &str) -> Result<Vec<ActorId>> {
        let _admission = self.admit().await?;
        async {
            actor::behavior(&self.registry, new_hash).await?;
            for id in self.actor_ids()? {
                self.sync_index(&id).await?;
            }
            let candidates = {
                let mut slot = self.names.lock().await;
                let index = connection(&mut slot, &self.dir, self.config.io).await?;
                let rows = actor::query(index, "SELECT id FROM who_runs WHERE behavior_hash=? ORDER BY id", [old_hash]).await?;
                rows.rows.into_iter().map(|row| row.get::<String>(0).map_err(anyhow::Error::from)).collect::<Result<Vec<_>>>()?
            };
            let mut promoted = Vec::new();
            for id in candidates {
                let candidate = self.open_actor(&id).await?;
                let mut conn = candidate.conn.lock().await;
                if actor::status(&conn).await? == Status::Stopped || actor::code(&conn).await?.hash != old_hash {
                    continue;
                }
                let behavior = actor::behavior(&self.registry, new_hash).await?;
                actor::promote(
                    &mut conn,
                    behavior.as_ref(),
                    author,
                    rationale,
                    &crate::effects::RuntimeEffects { node: self, external: self.effects.as_ref() },
                    Some(self),
                )
                .await
                .with_context(|| format!("actor {id} seq -1: promote_where stopped"))?;
                drop(conn);
                self.sync_index(&id).await?;
                promoted.push(id);
            }
            Ok::<Vec<ActorId>, anyhow::Error>(promoted)
        }
        .await
        .with_context(|| format!("actor <node> seq -1: promote_where {old_hash} -> {new_hash}"))
    }

    pub async fn info(&self, id: &str) -> Result<ActorInfo> {
        async {
            let actor = self.open_actor(id).await?;
            let conn = actor.conn.lock().await;
            let counts =
                actor::query(&conn, "SELECT COUNT(*),COUNT(CASE WHEN state='deferred' THEN 1 END) FROM inbox WHERE state!='done'", ())
                    .await?;
            let count = counts.rows.first().context("missing inbox counts")?;
            let links = actor::query(&conn, "SELECT peer FROM links ORDER BY peer", ())
                .await?
                .rows
                .into_iter()
                .map(|row| row.get::<String>(0).map_err(anyhow::Error::from))
                .collect::<Result<Vec<_>>>()?;
            let monitors = actor::query(&conn, "SELECT ref,target FROM monitors ORDER BY ref", ())
                .await?
                .rows
                .into_iter()
                .map(|row| Ok(MonitorInfo { reference: row.get(0)?, target: row.get(1)? }))
                .collect::<Result<Vec<_>>>()?;
            let children = actor::query(&conn, "SELECT id FROM children ORDER BY rowid", ())
                .await?
                .rows
                .into_iter()
                .map(|row| row.get::<String>(0).map_err(anyhow::Error::from))
                .collect::<Result<Vec<_>>>()?;
            let parent = actor::meta(&conn, "parent").await?;
            Ok::<ActorInfo, anyhow::Error>(ActorInfo {
                status: actor::status(&conn).await?,
                reason: actor::meta(&conn, "reason").await?,
                cursor: actor::cursor(&conn).await?,
                inbox_len: count.get(0)?,
                deferred_len: count.get(1)?,
                behavior_hash: actor::code(&conn).await?.hash,
                links,
                monitors,
                parent: if parent.is_empty() { None } else { Some(parent) },
                children,
            })
        }
        .await
        .with_context(|| format!("actor {id} seq -1: info"))
    }

    /// Actor lock precedes index lock, so a stale pre-stop observation cannot restore an index row.
    /// Forks never join the live index, including a fork subsequently marked stopped.
    pub(crate) async fn sync_index(&self, id: &str) -> Result<()> {
        async {
            let actor = self.open_actor(id).await?;
            let conn = actor.conn.lock().await;
            if !actor::query(&conn, "SELECT value FROM meta WHERE key='replay_source'", ()).await?.rows.is_empty()
                || actor::status(&conn).await? == Status::Fork { return Ok::<(), anyhow::Error>(()); }
            let stopped = actor::status(&conn).await? == Status::Stopped;
            let hash = actor::code(&conn).await?.hash;
            let mut slot = self.names.lock().await;
            let index = connection(&mut slot, &self.dir, self.config.io).await?;
            let tx = index.transaction().await?;
            if stopped {
                tx.execute("DELETE FROM names WHERE id=?", [id]).await?;
                tx.execute("DELETE FROM groups WHERE id=?", [id]).await?;
                tx.execute("DELETE FROM who_runs WHERE id=?", [id]).await?;
            } else {
                tx.execute("INSERT INTO who_runs(id,behavior_hash) VALUES (?,?) ON CONFLICT(id) DO UPDATE SET behavior_hash=excluded.behavior_hash", [id, hash.as_str()]).await?;
            }
            tx.commit().await?;
            Ok::<(), anyhow::Error>(())
        }.await.with_context(|| format!("actor {id} seq -1: sync node index"))
    }
}

async fn ensure_live(conn: &Connection) -> Result<()> {
    ensure!(!matches!(actor::status(conn).await?, Status::Stopped | Status::Fork), "registration requires a live actor");
    ensure!(
        actor::query(conn, "SELECT value FROM meta WHERE key='replay_source'", ()).await?.rows.is_empty(),
        "fork cannot enter live node index"
    );
    Ok::<(), anyhow::Error>(())
}

pub(crate) async fn connection<'a>(slot: &'a mut Option<Connection>, dir: &Path, io: crate::Io) -> Result<&'a mut Connection> {
    if slot.is_none() {
        let conn = actor::connect(&dir.join("_node.db"), io).await?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS names(name TEXT PRIMARY KEY,id TEXT NOT NULL);\n\
            CREATE TABLE IF NOT EXISTS groups(\"group\" TEXT NOT NULL,id TEXT NOT NULL,PRIMARY KEY(\"group\",id));\n\
            CREATE TABLE IF NOT EXISTS who_runs(id TEXT PRIMARY KEY,behavior_hash TEXT NOT NULL);",
        )
        .await?;
        *slot = Some(conn);
    }
    slot.as_mut().context("missing node index connection")
}
