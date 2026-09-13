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
pub(crate) async fn complete_at(conn: &Connection, seq: i64, epoch: i64, revision: Option<i64>) -> Result<Completion> {
    let epoch = epoch.checked_add(1).context("commit epoch overflow")?;
    conn.execute("UPDATE inbox SET state='done' WHERE seq=?", [seq]).await?;
    let rows = actor::query(conn,
        "SELECT cursor, NOT EXISTS(SELECT 1 FROM inbox WHERE state='done' AND seq>cursor) FROM (SELECT COALESCE(MIN(seq)-1,(SELECT COALESCE(MAX(seq),0) FROM inbox)) AS cursor FROM inbox WHERE state!='done')", ()).await?;
    let row = rows.rows.first().context("missing completion aggregate")?;
    let completion = Completion { cursor: row.get(0)?, epoch, boundary: row.get::<i64>(1)? != 0 };
    conn.execute(
        "INSERT OR REPLACE INTO meta(key,value) SELECT 'commit_epoch',?1 UNION ALL SELECT 'commit_order:' || ?1,?2 UNION ALL SELECT 'cursor',?3 UNION ALL SELECT 'boundary:' || ?3,?1 WHERE ?4 UNION ALL SELECT 'code_at:' || ?2,?5 WHERE ?5 IS NOT NULL",
        turso::params![epoch.to_string(), seq.to_string(), completion.cursor.to_string(), i64::from(completion.boundary), revision.map(|value| value.to_string())],
    ).await?;
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
