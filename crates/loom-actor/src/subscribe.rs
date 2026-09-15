//! Subscription operations and frames leave only through committed outboxes.
use crate::{Cap, Ctx, Node, Rights, Trap, actor, cdc};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Operation {
    Subscribe { id: String, subscriber: String, table: String },
    Unsubscribe { id: String, subscriber: String },
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Envelope {
    pub key: String,
    pub frame: Value,
}
#[derive(Debug, Serialize)]
pub struct Subscription {
    pub id: String,
    pub subscriber: String,
    pub table: String,
    pub after_change_id: i64,
}

impl Ctx<'_> {
    pub async fn subscribe(&mut self, cap: &Cap, table: &str) -> Result<String, Trap> {
        self.authorize(cap, Rights::INSPECT, "subscribe").await?;
        let idx = self.next_index()?;
        let id = format!("{}:{}:{}:{idx}", cap.target, crate::ids::incarnation(self.actor_id, self.generation), self.seq);
        let op = Operation::Subscribe { id: id.clone(), subscriber: self.actor_id.into(), table: table.into() };
        let bytes = serde_json::to_vec(&op).map_err(|e| self.runtime(e))?;
        self.outbox(idx, &format!("sub:{}", cap.target), &bytes).await?;
        Ok(id)
    }

    pub async fn unsubscribe(&mut self, id: &str) -> Result<(), Trap> {
        let target = id.split(':').next().ok_or_else(|| Trap::new("unsubscribe: invalid subscription id"))?;
        let op = Operation::Unsubscribe { id: id.into(), subscriber: self.actor_id.into() };
        let bytes = serde_json::to_vec(&op).map_err(|e| self.runtime(e))?;
        self.control("sub", target, &bytes).await
    }
}

/// Origin metadata is retained with history; CDC snapshot compaction removes old origin entries.
pub(crate) async fn record_origin(conn: &turso::Connection, id: &str, seq: i64) -> Result<()> {
    let rows = actor::query(conn, "SELECT MAX(change_txn_id) FROM turso_cdc", ()).await?;
    let txn: i64 = rows.rows.first().context("CDC missing transaction")?.get(0)?;
    let rows = actor::query(conn, "SELECT key,msg FROM inbox WHERE seq=?", [seq]).await?;
    let row = rows.rows.first().with_context(|| format!("actor {id} seq {seq}: CDC origin missing inbox"))?;
    let key: String = row.get(0)?;
    let bytes: Vec<u8> = row.get(1)?;
    let cause = match serde_json::from_slice::<Value>(&bytes) {
        Ok(frame) if frame.get("type").and_then(Value::as_str) == Some("delta") => {
            // Only the immediately handled delta's key propagates; never inherit its cause.
            let key = frame
                .get("key")
                .and_then(Value::as_str)
                .with_context(|| format!("actor {id} seq {seq}: delta origin missing string key"))?;
            json!(key)
        }
        _ => Value::Null,
    };
    let origin = json!({"seq":seq,"key":key,"cause":cause});
    actor::set_meta(conn, &format!("cdc_origin:{txn}"), &origin.to_string()).await
}

pub(crate) async fn enqueue(conn: &turso::Connection, subscriber: &str, key: &str, frame: Value) -> Result<()> {
    let target = if subscriber.starts_with("ws:") { subscriber.to_owned() } else { format!("frame:{subscriber}") };
    let bytes = serde_json::to_vec(&Envelope { key: key.into(), frame })?;
    actor::enqueue(conn, actor::cursor(conn).await?, &target, &bytes).await
}

async fn snapshot(conn: &turso::Connection, source: &str, subscription: &Subscription, resnapshot: bool) -> Result<i64> {
    let high = cdc::high_water(conn).await?;
    let seq = actor::cursor(conn).await?;
    if resnapshot {
        enqueue(
            conn,
            &subscription.subscriber,
            &format!("resnapshot:{}:{high}", subscription.id),
            json!({"type":"resnapshot","source":source,"seq":seq,"key":"control","table":subscription.table}),
        )
        .await?;
    }
    let rows = cdc::snapshot(conn, &subscription.table).await?;
    enqueue(
        conn,
        &subscription.subscriber,
        &format!("snapshot:{}:{high}", subscription.id),
        json!({"type":"snapshot","source":source,"seq":seq,"key":"control","table":subscription.table,
            "change_id":high,"rows":rows}),
    )
    .await?;
    Ok(high)
}

impl Node {
    /// A restarted node cannot retain subscriptions owned by sockets or forgotten memory actors.
    pub(crate) async fn prune_subscribers(&self) -> Result<()> {
        for source in self.actor_ids()? {
            let owner = self.open_actor(&source).await?;
            let rows = actor::query(
                &*owner.conn.lock().await,
                "SELECT id,subscriber FROM subscribers WHERE subscriber LIKE 'ws:%'
                 OR id IN (SELECT substr(key,15) FROM meta WHERE key LIKE 'ephemeral_sub:%')",
                (),
            )
            .await?;
            for row in rows.rows {
                let id: String = row.get(0)?;
                let subscriber: String = row.get(1)?;
                if !subscriber.starts_with("ws:") && self.is_memory(&subscriber)? {
                    continue;
                }
                self.host_subscription(&source, Operation::Unsubscribe { id, subscriber }).await?;
            }
        }
        Ok(())
    }

    /// Even a kill that bypasses terminate drains unsubscribe operations through the stopped actor's pump.
    pub(crate) async fn close_actor_subscriptions(&self, id: &str) -> Result<()> {
        let owner = self.open_actor(id).await?;
        {
            let conn = owner.conn.lock().await;
            if actor::status(&conn).await? != crate::Status::Stopped
                || !actor::query(&conn, "SELECT value FROM meta WHERE key='subscriptions_closed'", ()).await?.rows.is_empty()
            {
                return Ok(());
            }
        }
        let mut operations = Vec::new();
        for source in self.actor_ids()? {
            // This also runs during Node::close under exclusive admission.
            // Use the admitted connection path; public subscriptions/open would
            // request shared admission and deadlock shutdown on itself.
            let source_actor = self.open_actor(&source).await?;
            let existing = subscriptions(&*source_actor.conn.lock().await).await?;
            for sub in existing {
                if sub.subscriber == id {
                    operations.push(Closing { source: source.clone(), id: sub.id });
                }
            }
        }
        let mut conn = owner.conn.lock().await;
        let tx = conn.transaction().await?;
        for closing in operations {
            let operation = Operation::Unsubscribe { id: closing.id, subscriber: id.into() };
            actor::enqueue(&tx, actor::cursor(&tx).await?, &format!("sub:{}", closing.source), &serde_json::to_vec(&operation)?).await?;
        }
        // Reset deletes this marker with the old runtime state; stopped actors cannot subscribe again.
        actor::set_meta(&tx, "subscriptions_closed", "true").await?;
        self.commit_control(id, tx).await?;
        // The unsubscribe rows sit in this actor's outbox; its pump must run.
        self.wake_actor(id)
    }

    /// The returned receiver owns one socket's frames; close_stream is its leaver.
    pub async fn open_stream(&self) -> Result<HostStream> {
        let id = format!("ws:{}", crate::ids::root());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        self.streams.lock().await.insert(id.clone(), sender);
        Ok(HostStream { id, receiver })
    }

    pub async fn subscribe_stream(&self, stream: &str, cap: &Cap, table: &str) -> Result<String> {
        self.check_cap(cap, Rights::INSPECT, "subscribe").await?;
        ensure!(self.streams.lock().await.contains_key(stream), "subscribe: unknown WebSocket subscriber");
        let id = format!("{}:{stream}:{}", cap.target, crate::ids::root());
        self.host_subscription(&cap.target, Operation::Subscribe { id: id.clone(), subscriber: stream.into(), table: table.into() })
            .await?;
        self.pump(&cap.target).await?;
        Ok(id)
    }

    pub async fn close_stream(&self, stream: &str) -> Result<()> {
        self.streams.lock().await.remove(stream);
        for actor in self.actor_ids()? {
            for sub in self.subscriptions(&actor).await? {
                if sub.subscriber == stream {
                    self.host_subscription(&actor, Operation::Unsubscribe { id: sub.id, subscriber: stream.into() }).await?;
                }
            }
        }
        Ok(())
    }

    async fn host_subscription(&self, target: &str, operation: Operation) -> Result<()> {
        let root = self.root();
        let owner = self.open_actor(&root).await?;
        let mut conn = owner.conn.lock().await;
        let tx = conn.transaction().await?;
        actor::enqueue(&tx, actor::cursor(&tx).await?, &format!("sub:{target}"), &serde_json::to_vec(&operation)?).await?;
        self.commit_control(&root, tx).await?;
        drop(conn);
        self.pump(&root).await?;
        Ok(())
    }

    pub async fn subscriptions(&self, id: &str) -> Result<Vec<Subscription>> {
        let actor = self.open(id).await?;
        subscriptions(&*actor.conn.lock().await).await.with_context(|| format!("actor {id} seq -1: subscriptions"))
    }

    pub(crate) async fn apply_subscription(&self, sender: &str, target: &str, bytes: &[u8], key: &str) -> Result<()> {
        let operation: Operation = serde_json::from_slice(bytes)?;
        let owner = self.open_actor(target).await?;
        let mut conn = owner.conn.lock().await;
        let tx = conn.transaction().await?;
        if crate::supervision::applied(&tx, key).await? {
            tx.rollback().await?;
            return Ok(());
        }
        match operation {
            Operation::Subscribe { id, subscriber, table } => {
                ensure!(subscriber == sender || (sender == self.root() && subscriber.starts_with("ws:")), "subscribe owner mismatch");
                if subscriber.starts_with("ws:") && !self.streams.lock().await.contains_key(&subscriber) {
                    // A socket can close before its queued subscribe reaches this pump, including after restart.
                    actor::set_meta(&tx, &format!("applied:{key}"), "1").await?;
                    return self.commit_control(target, tx).await;
                }
                ensure!(
                    !crate::schema::SYSTEM_TABLES.contains(&table.as_str())
                        && !table.starts_with("sqlite_")
                        && !table.starts_with("turso_"),
                    "subscribe refuses runtime table {table}"
                );
                let exists = actor::query(&tx, "SELECT name FROM sqlite_schema WHERE type='table' AND name=?", [table.as_str()]).await?;
                ensure!(!exists.rows.is_empty(), "subscribe table {table} does not exist");
                let subscription = Subscription { id, subscriber, table, after_change_id: 0 };
                let high = snapshot(&tx, target, &subscription, false).await?;
                // Unsubscribe and close_stream remove subscriber rows; fanout advances their cursor.
                tx.execute(
                    "INSERT OR IGNORE INTO subscribers(id,subscriber,\"table\",after_change_id) VALUES (?,?,?,?)",
                    turso::params![subscription.id.as_str(), subscription.subscriber.as_str(), subscription.table, high],
                )
                .await?;
                if self.is_memory(&subscription.subscriber)? {
                    // Unsubscribe removes this restart-cleanup marker with its subscriber row.
                    actor::set_meta(&tx, &format!("ephemeral_sub:{}", subscription.id), "true").await?;
                }
            }
            Operation::Unsubscribe { id, subscriber } => {
                ensure!(subscriber == sender || sender == self.root(), "unsubscribe owner mismatch");
                tx.execute("DELETE FROM subscribers WHERE id=? AND subscriber=?", [id.as_str(), subscriber.as_str()]).await?;
                tx.execute("DELETE FROM meta WHERE key=?", [format!("ephemeral_sub:{id}")]).await?;
            }
        }
        actor::set_meta(&tx, &format!("applied:{key}"), "1").await?;
        self.commit_control(target, tx).await?;
        // A new subscriber's snapshot frame sits in the target's outbox; its pump must run.
        self.wake_actor(target)
    }

    pub(crate) async fn fanout(&self, id: &str) -> Result<()> {
        let owner = self.open_actor(id).await?;
        let mut conn = owner.conn.lock().await;
        self.fanout_on(id, &mut conn).await
    }

    pub(crate) async fn fanout_on(&self, id: &str, conn: &mut turso::Connection) -> Result<()> {
        if actor::status(conn).await? == crate::Status::Fork {
            return Ok(());
        }
        let subscribers = subscriptions(conn).await?;
        if subscribers.is_empty() {
            return Ok(());
        }
        let tx = conn.transaction().await?;
        let floor: i64 = actor::meta(&tx, "cdc_floor").await?.parse()?;
        let high = cdc::high_water(&tx).await?;
        struct Batch {
            subscriber: String,
            txn: i64,
            rows: Vec<cdc::DeltaRow>,
        }
        let mut batches: BTreeMap<String, Batch> = BTreeMap::new();
        let mut changed = false;
        for sub in subscribers {
            if sub.after_change_id < floor {
                let high = snapshot(&tx, id, &sub, true).await?;
                tx.execute("UPDATE subscribers SET after_change_id=? WHERE id=?", turso::params![high, sub.id]).await?;
                changed = true;
                continue;
            }
            let rows = actor::query(
                &tx,
                "SELECT change_id,change_type,table_name,id,before,after,updates,change_txn_id FROM turso_cdc
                 WHERE change_id>? AND change_id<=? AND table_name=? AND change_type!=2 ORDER BY change_id",
                turso::params![sub.after_change_id, high, sub.table],
            )
            .await?;
            if rows.rows.is_empty() {
                continue;
            }
            changed = true;
            for row in rows.rows {
                let txn: i64 = row.get(7)?;
                let batch = batches.entry(format!("{txn:020}:{}", sub.subscriber)).or_insert_with(|| Batch {
                    subscriber: sub.subscriber.clone(),
                    txn,
                    rows: Vec::new(),
                });
                let delta = cdc::delta(&tx, &row).await?;
                if !batch.rows.iter().any(|row| row.change_id == delta.change_id) {
                    batch.rows.push(delta);
                }
            }
            tx.execute("UPDATE subscribers SET after_change_id=? WHERE id=?", turso::params![high, sub.id]).await?;
        }
        for mut batch in batches.into_values() {
            batch.rows.sort_by_key(|row| row.change_id);
            let origin = actor::query(&tx, "SELECT value FROM meta WHERE key=?", [format!("cdc_origin:{}", batch.txn)]).await?;
            let origin: Value = match origin.rows.first() {
                Some(row) => serde_json::from_str(&row.get::<String>(0)?)?,
                // Control transactions use negative CDC transaction identities to avoid key collisions at an unchanged inbox cursor.
                None => json!({"seq":-batch.txn,"key":"control"}),
            };
            let seq = origin["seq"].as_i64().context("CDC origin missing seq")?;
            let frame = json!({"type":"delta","source":id,"seq":seq,"key":origin["key"],
                "cause":origin["cause"],"rows":batch.rows});
            enqueue(&tx, &batch.subscriber, &format!("delta:{id}:{seq}:{}", batch.subscriber), frame).await?;
        }
        if changed {
            self.commit_control(id, tx).await?;
            // Delta frames sit in this actor's outbox; when fanout runs outside a step, wake its pump.
            self.wake_actor(id)
        } else {
            tx.rollback().await.map_err(Into::into)
        }
    }
}

pub struct HostStream {
    pub id: String,
    pub receiver: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
}

struct Closing {
    source: String,
    id: String,
}

async fn subscriptions(conn: &turso::Connection) -> Result<Vec<Subscription>> {
    let rows = actor::query(conn, "SELECT id,subscriber,\"table\",after_change_id FROM subscribers ORDER BY id", ()).await?;
    rows.rows
        .into_iter()
        .map(|row| Ok(Subscription { id: row.get(0)?, subscriber: row.get(1)?, table: row.get(2)?, after_change_id: row.get(3)? }))
        .collect()
}
