//! A contiguous cursor plus explicit row state supports selective receive.
use crate::{Trap, actor};
use anyhow::{Context, Result};
use turso::Connection;

pub(crate) const FIRST_PENDING: &str = "SELECT seq FROM inbox INDEXED BY inbox_state_seq WHERE state='pending' ORDER BY seq LIMIT 1";
pub(crate) const FIRST_DEFERRED: &str = "SELECT seq FROM inbox INDEXED BY inbox_state_seq WHERE state='deferred' ORDER BY seq LIMIT 1";
pub(crate) const LAST_INBOX: &str = "SELECT seq FROM inbox ORDER BY seq DESC LIMIT 1";
pub(crate) const DONE_ABOVE: &str = "SELECT 1 FROM inbox INDEXED BY inbox_state_seq WHERE state='done' AND seq>? LIMIT 1";
pub(crate) const NEXT_DEFERRED: &str =
    "SELECT seq,msg,sender FROM inbox INDEXED BY inbox_state_epoch_seq WHERE state='deferred' AND defer_epoch<? ORDER BY seq LIMIT 1";
pub(crate) const NEXT_PENDING: &str =
    "SELECT seq,msg,sender FROM inbox INDEXED BY inbox_state_seq WHERE state='pending' ORDER BY seq LIMIT 1";
pub(crate) const NEXT_OTHER_DEFERRED: &str =
    "SELECT seq,msg,sender,defer_epoch FROM inbox INDEXED BY inbox_state_seq WHERE state='deferred' ORDER BY seq LIMIT 1";

#[cfg(test)]
tokio::task_local! {
    pub(crate) static STATEMENTS: std::cell::Cell<usize>;
}

fn count_statement() {
    #[cfg(test)]
    let _ = STATEMENTS.try_with(|count| count.set(count.get() + 1));
}

async fn query(conn: &Connection, sql: &str, params: impl turso::IntoParams) -> Result<crate::Rows> {
    count_statement();
    actor::query(conn, sql, params).await
}

async fn first_seq(conn: &Connection, sql: &str) -> Result<Option<i64>> {
    query(conn, sql, ()).await?.rows.first().map(|row| row.get(0).map_err(Into::into)).transpose()
}

/// Read persisted rows in the caller's transaction; no cursor cache to invalidate.
async fn contiguous_cursor(conn: &Connection) -> Result<i64> {
    let pending = first_seq(conn, FIRST_PENDING).await?;
    let deferred = first_seq(conn, FIRST_DEFERRED).await?;
    match pending.into_iter().chain(deferred).min() {
        Some(seq) => seq.checked_sub(1).context("cursor underflow"),
        None => Ok(first_seq(conn, LAST_INBOX).await?.unwrap_or(0)),
    }
}

pub(crate) async fn next(conn: &Connection) -> Result<Option<actor::Message>> {
    next_at(conn, actor::meta(conn, "commit_epoch").await?.parse()?).await
}

pub(crate) async fn next_at(conn: &Connection, epoch: i64) -> Result<Option<actor::Message>> {
    let deferred = query(conn, NEXT_OTHER_DEFERRED, ()).await?;
    let rows = if let Some(first) = deferred.rows.first() {
        if first.get::<i64>(3)? < epoch {
            // The first deferred row is eligible, hence also the global winner.
            deferred
        } else {
            // Keep the epoch-range predicate exact for mixed epoch layouts.
            // The epoch index excludes pending/done rows, but eligible deferred
            // rows still require a seq sort. This branch is not logarithmic.
            let eligible = query(conn, NEXT_DEFERRED, [epoch]).await?;
            if eligible.rows.is_empty() {
                let pending = query(conn, NEXT_PENDING, ()).await?;
                if pending.rows.is_empty() { deferred } else { pending }
            } else {
                eligible
            }
        }
    } else {
        query(conn, NEXT_PENDING, ()).await?
    };
    rows.rows
        .first()
        .map(|row| {
            let sender: String = row.get(2)?;
            Ok(actor::Message {
                seq: row.get(0)?,
                msg: row.get(1)?,
                sender: visible_sender(sender),
            })
        })
        .transpose()
}

/// The sender a behavior sees: an actor id (`a0…`), a driver (`drv:…`), or `"external"`
/// for a message sent through the node API. Host-internal rows (root init, lifecycle
/// messages) carry other markers and read as `None`.
pub(crate) fn visible_sender(sender: String) -> Option<String> {
    if sender.starts_with("a0") || sender.starts_with("drv:") || sender == crate::EXTERNAL_SENDER { Some(sender) } else { None }
}

pub(crate) async fn refresh_cursor(conn: &Connection) -> Result<i64> {
    let cursor = contiguous_cursor(conn).await?;
    actor::set_meta(conn, "cursor", &cursor.to_string()).await?;
    Ok(cursor)
}

pub(crate) struct Completion {
    pub cursor: i64,
    pub epoch: i64,
    pub boundary: bool,
}

pub(crate) async fn complete(conn: &Connection, seq: i64) -> Result<()> {
    let epoch = actor::meta(conn, "commit_epoch").await?.parse()?;
    complete_at(conn, seq, epoch, None).await?;
    Ok(())
}

/// The caller holds the connection lock and supplies the last committed epoch.
/// Failed transactions discard this result; only a committed attempt advances it.
/// Keep completion SQL flat: seek inbox indexes, then bind metadata
/// with VALUES. A completion SQL error rolls back the handler's domain writes
/// and enters supervision after retries; an Ok drain is not proof of a commit.
pub(crate) async fn complete_at(conn: &Connection, seq: i64, epoch: i64, revision: Option<i64>) -> Result<Completion> {
    let epoch = epoch.checked_add(1).context("commit epoch overflow")?;
    count_statement();
    conn.execute("UPDATE inbox SET state='done' WHERE seq=?", [seq]).await?;
    let cursor = contiguous_cursor(conn).await?;
    let boundary = query(conn, DONE_ABOVE, [cursor]).await?.rows.is_empty();
    let completion = Completion { cursor, epoch, boundary };
    let mut sql = String::from("INSERT OR REPLACE INTO meta(key,value) VALUES (?,?),(?,?),(?,?)");
    let mut params = vec![
        turso::Value::Text("commit_epoch".into()),
        turso::Value::Text(epoch.to_string()),
        turso::Value::Text(format!("commit_order:{epoch}")),
        turso::Value::Text(seq.to_string()),
        turso::Value::Text("cursor".into()),
        turso::Value::Text(cursor.to_string()),
    ];
    if completion.boundary {
        sql.push_str(",(?,?)");
        params.push(turso::Value::Text(format!("boundary:{cursor}")));
        params.push(turso::Value::Text(epoch.to_string()));
    }
    if let Some(revision) = revision {
        sql.push_str(",(?,?)");
        params.push(turso::Value::Text(format!("code_at:{seq}")));
        params.push(turso::Value::Text(revision.to_string()));
    }
    count_statement();
    conn.execute(sql, params).await?;
    Ok(completion)
}

pub(crate) async fn defer(conn: &mut Connection, id: &str, seq: i64, node: Option<&crate::Node>) -> Result<(), Trap> {
    let result: Result<bool> = async {
        let tx = conn.transaction().await?;
        let epoch: i64 = actor::meta(&tx, "commit_epoch").await?.parse()?;
        let rows = actor::query(&tx, "SELECT defer_epoch,defer_count FROM inbox WHERE seq=?", [seq]).await?;
        let row = rows.rows.first().context("deferred inbox row missing")?;
        let count = if row.get::<i64>(0)? == epoch { row.get::<i64>(1)?.checked_add(1).context("defer count overflow")? } else { 1 };
        tx.execute("UPDATE inbox SET state='deferred',defer_epoch=?,defer_count=? WHERE seq=?", turso::params![epoch, count, seq]).await?;
        if let Some(node) = node {
            node.commit_control(id, tx).await?;
        } else {
            tx.commit().await?;
        }
        Ok(count >= 2)
    }
    .await;
    match result {
        Ok(false) => Ok(()),
        Ok(true) => Err(Trap::new(format!("actor {id} seq {seq}: deferred twice without an intervening commit"))),
        Err(error) => Err(Trap { message: format!("actor {id} seq {seq}: {error:#}"), runtime: true, durability: false }),
    }
}
