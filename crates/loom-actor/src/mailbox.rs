//! A contiguous cursor plus explicit row state supports selective receive.
use crate::{Trap, actor};
use anyhow::{Context, Result};
use turso::Connection;

pub(crate) async fn next(conn: &Connection) -> Result<Option<actor::Message>> {
    next_at(conn, actor::meta(conn, "commit_epoch").await?.parse()?).await
}

pub(crate) async fn next_at(conn: &Connection, epoch: i64) -> Result<Option<actor::Message>> {
    let rows = actor::query(conn, "SELECT seq,msg,sender FROM inbox WHERE state!='done' ORDER BY CASE WHEN state='deferred' AND defer_epoch<? THEN 0 WHEN state='pending' THEN 1 ELSE 2 END,seq LIMIT 1", [epoch]).await?;
    rows.rows
        .first()
        .map(|row| {
            let sender: String = row.get(2)?;
            Ok(actor::Message {
                seq: row.get(0)?,
                msg: row.get(1)?,
                sender: if sender.starts_with("a0") || sender.starts_with("drv:") { Some(sender) } else { None },
            })
        })
        .transpose()
}

pub(crate) async fn refresh_cursor(conn: &Connection) -> Result<i64> {
    let rows =
        actor::query(conn, "SELECT COALESCE(MIN(seq)-1,(SELECT COALESCE(MAX(seq),0) FROM inbox)) FROM inbox WHERE state!='done'", ())
            .await?;
    let cursor: i64 = rows.rows.first().context("missing cursor aggregate")?.get(0)?;
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
/// Keep completion SQL flat: aggregate inbox rows directly, then bind metadata
/// with VALUES. A completion SQL error rolls back the handler's domain writes
/// and enters supervision after retries; an Ok drain is not proof of a commit.
pub(crate) async fn complete_at(conn: &Connection, seq: i64, epoch: i64, revision: Option<i64>) -> Result<Completion> {
    let epoch = epoch.checked_add(1).context("commit epoch overflow")?;
    conn.execute("UPDATE inbox SET state='done' WHERE seq=?", [seq]).await?;
    let rows = actor::query(conn,
        "SELECT COALESCE(MIN(CASE WHEN state!='done' THEN seq END)-1,MAX(seq),0), COALESCE(MAX(CASE WHEN state='done' THEN seq END),0), COUNT(CASE WHEN state='done' THEN 1 END) FROM inbox", ()).await?;
    let row = rows.rows.first().context("missing completion aggregate")?;
    let cursor = row.get::<i64>(0)?;
    let completion = Completion { cursor, epoch, boundary: row.get::<i64>(2)? == 0 || row.get::<i64>(1)? <= cursor };
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
