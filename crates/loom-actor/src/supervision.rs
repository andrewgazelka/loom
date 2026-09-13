use crate::{ActorId, ChildState, Node, Status, TreeEntry, actor};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeSet, VecDeque};
use turso::Connection;

impl Node {
    pub async fn tree(&self, root: &str) -> Result<Vec<TreeEntry>> {
        self.tree_inner(root).await.with_context(|| format!("actor {} seq -1: tree", root))
    }
    pub(crate) async fn relate(&self, sender: &str, target: &str, kind: &str, msg: &[u8], key: &str) -> Result<()> {
        match kind {
            "revoke" => {
                let cap: crate::Cap = serde_json::from_slice(msg)?;
                ensure!(cap.target == target, "revoke target mismatch");
                self.revoke_cap(&cap).await.map_err(Into::into)
            }
            "promote" => {
                let operation: crate::cap_ops::Promotion = serde_json::from_slice(msg)?;
                self.promote_inner(target, &operation.hash, &operation.author, &operation.rationale).await
            }
            "stop" => self.stop_unlocked(target, std::str::from_utf8(msg)?, key, sender).await,
            "shutdown" => self.shutdown(sender, target, key).await,
            "link" | "unlink" => {
                let key_pair = if sender <= target { format!("link:{sender}:{target}") } else { format!("link:{target}:{sender}") };
                let _relation = self.guard(&key_pair).await;
                for owner in [sender, target] {
                    let peer = if owner == sender { target } else { sender };
                    let actor = self.open_actor(owner).await?;
                    let mut conn = actor.conn.lock().await;
                    let tx = conn.transaction().await?;
                    if !applied(&tx, key).await? {
                        if kind == "link" {
                            tx.execute("INSERT OR IGNORE INTO links(peer) VALUES (?)", [peer]).await?;
                        } else {
                            tx.execute("DELETE FROM links WHERE peer=?", [peer]).await?;
                        }
                        actor::set_meta(&tx, &format!("applied:{key}"), "1").await?;
                    }
                    self.commit_control(owner, tx).await?;
                }
                Ok(())
            }
            "monitor" => self.monitor(sender, target, std::str::from_utf8(msg)?).await,
            "demonitor" => {
                let payload: serde_json::Value = serde_json::from_slice(msg)?;
                let destination = payload["target"].as_str().context("demonitor target missing")?;
                let flush = payload["flush"].as_bool().context("demonitor flush missing")?;
                self.demonitor(sender, target, destination).await?;
                if flush {
                    let owner = self.open_actor(sender).await?;
                    let mut conn = owner.conn.lock().await;
                    let tx = conn.transaction().await?;
                    tx.execute("DELETE FROM inbox WHERE key=? AND state!='done'", [format!("down:{target}")]).await?;
                    crate::mailbox::refresh_cursor(&tx).await?;
                    self.commit_control(sender, tx).await?;
                }
                Ok(())
            }
            "down" | "exit" => self.signal(sender, target, kind, msg, key).await,
            _ => anyhow::bail!("actor {sender} seq -1: unknown outbox target {kind}:{target}"),
        }
    }

    pub(crate) async fn monitor(&self, watcher: &str, target: &str, reference: &str) -> Result<()> {
        let _relation = self.guard(&format!("monitor:{watcher}:{reference}")).await;
        let a = self.open_actor(watcher).await?;
        let b = self.open_actor(target).await?;
        {
            let mut conn = a.conn.lock().await;
            if applied(&conn, &format!("monitor:{reference}")).await? {
                return Ok(());
            }
            let tx = conn.transaction().await?;
            tx.execute("INSERT OR IGNORE INTO monitors(ref,target) VALUES (?,?)", [reference, target]).await?;
            self.commit_control(watcher, tx).await?;
        }
        let mut conn = b.conn.lock().await;
        let tx = conn.transaction().await?;
        tx.execute("INSERT OR IGNORE INTO monitored_by(ref,watcher) VALUES (?,?)", [reference, watcher]).await?;
        let status = actor::status(&tx).await?;
        if status == Status::Stopped {
            let generation = actor::meta(&tx, "generation").await?;
            let counter = actor::meta(&tx, "event_counter").await?;
            let reason = actor::meta(&tx, "reason").await?;
            let msg = serde_json::json!({"type":"down","ref":reference,"from":target,"reason":reason,
                "generation":generation.parse::<i64>()?,"event":format!("{target}:{generation}:{counter}")});
            actor::enqueue(&tx, actor::cursor(&tx).await?, &format!("down:{watcher}"), &serde_json::to_vec(&msg)?).await?;
        }
        self.commit_control(target, tx).await?;
        // The DOWN for an already-stopped target sits in the target's outbox: its pump must run.
        self.wake_actor(target)?;
        Ok(())
    }

    pub(crate) async fn demonitor(&self, watcher: &str, reference: &str, target: &str) -> Result<()> {
        let _relation = self.guard(&format!("monitor:{watcher}:{reference}")).await;
        let a = self.open_actor(watcher).await?;
        let mut conn = a.conn.lock().await;
        let tx = conn.transaction().await?;
        let target = if target.is_empty() {
            let rows = actor::query(
                &tx,
                "SELECT target FROM monitors WHERE ref=? UNION ALL SELECT value FROM meta WHERE key=? LIMIT 1",
                [reference, &format!("monitor_target:{reference}")],
            )
            .await?;
            match rows.rows.first() {
                Some(row) => row.get::<String>(0)?,
                None => String::new(),
            }
        } else {
            target.to_owned()
        };
        if !target.is_empty() {
            actor::set_meta(&tx, &format!("monitor_target:{reference}"), &target).await?;
        }
        tx.execute("DELETE FROM monitors WHERE ref=?", [reference]).await?;
        actor::set_meta(&tx, &format!("applied:monitor:{reference}"), "1").await?;
        self.commit_control(watcher, tx).await?;
        drop(conn);
        if !target.is_empty() {
            let owner = self.open_actor(&target).await?;
            let mut conn = owner.conn.lock().await;
            let tx = conn.transaction().await?;
            tx.execute("DELETE FROM monitored_by WHERE ref=?", [reference]).await?;
            self.commit_control(&target, tx).await?;
        }
        Ok(())
    }

    async fn signal(&self, sender: &str, target: &str, kind: &str, msg: &[u8], key: &str) -> Result<()> {
        let payload: serde_json::Value = serde_json::from_slice(msg)?;
        let reason = payload["reason"].as_str().context("signal requires reason")?;
        if kind == "down" && self.complete_call(target, sender, key, msg).await? {
            return Ok(());
        }
        let receiver = self.open_actor(target).await?;
        let mut conn = receiver.conn.lock().await;
        let tx = conn.transaction().await?;
        if kind == "down" {
            let reference = payload["ref"].as_str().context("down requires ref")?;
            let done = format!("monitor:{reference}");
            if !applied(&tx, &done).await? {
                actor::inject(&tx, &format!("down:{reference}"), sender, msg).await?;
                tx.execute("DELETE FROM monitors WHERE ref=?", [reference]).await?;
                actor::set_meta(&tx, &format!("applied:{done}"), "1").await?;
            }
            self.commit_control(target, tx).await?;
            drop(conn);
            let owner = self.open_actor(sender).await?;
            let mut conn = owner.conn.lock().await;
            let tx = conn.transaction().await?;
            tx.execute("DELETE FROM monitored_by WHERE ref=?", [reference]).await?;
            self.commit_control(sender, tx).await?;
        } else {
            if applied(&tx, key).await? {
                self.commit_control(target, tx).await?;
                return Ok(());
            }
            let trapping: bool = actor::meta(&tx, "trap_exit").await?.parse()?;
            let initiator = payload["initiator"].as_str().unwrap_or("");
            if reason == "kill" || (!trapping && reason != "normal") {
                tx.rollback().await?;
                drop(conn);
                self.stop_unlocked(target, reason, key, initiator).await?;
            } else {
                if trapping {
                    actor::inject(&tx, key, sender, msg).await?;
                }
                actor::set_meta(&tx, &format!("applied:{key}"), "1").await?;
                self.commit_control(target, tx).await?;
            }
        }
        self.wake_actor(target)?;
        Ok(())
    }

    pub(crate) async fn child_state(&self, id: &str) -> Result<ChildState> {
        let actor = self.open_actor(id).await?;
        let conn = actor.conn.lock().await;
        let code = actor::code(&conn).await?;
        let poison = actor::query(&conn, "SELECT value FROM meta WHERE key='poison_revision'", ()).await?;
        Ok(ChildState {
            id: id.into(),
            status: actor::status(&conn).await?,
            behavior_hash: code.hash,
            generation: actor::meta(&conn, "generation").await?.parse()?,
            revision: code.revision,
            poison_revision: match poison.rows.first() {
                Some(row) => Some(row.get::<String>(0)?.parse()?),
                None => None,
            },
        })
    }

    async fn tree_inner(&self, root: &str) -> Result<Vec<TreeEntry>> {
        struct Visit {
            id: ActorId,
            depth: usize,
        }
        let mut queue = VecDeque::from([Visit { id: root.into(), depth: 0 }]);
        let mut seen = BTreeSet::new();
        let mut result = Vec::new();
        while let Some(visit) = queue.pop_front() {
            ensure!(seen.insert(visit.id.clone()), "actor {} seq -1: children contain a cycle", visit.id);
            let state = self.child_state(&visit.id).await?;
            let actor = self.open_actor(&visit.id).await?;
            let rows = actor::query(&*actor.conn.lock().await, "SELECT id FROM children ORDER BY rowid", ()).await?;
            for row in rows.rows {
                queue.push_back(Visit { id: row.get(0)?, depth: visit.depth + 1 });
            }
            result.push(TreeEntry { depth: visit.depth, id: visit.id, status: state.status, behavior_hash: state.behavior_hash });
        }
        Ok(result)
    }
}

pub(crate) async fn applied(conn: &Connection, key: &str) -> Result<bool> {
    Ok(!actor::query(conn, "SELECT value FROM meta WHERE key=?", [format!("applied:{key}")]).await?.rows.is_empty())
}
