//! Logical snapshots keep ephemeral history inside the node, never in a store prefix.
use crate::{Node, actor, sql_value::SqlValue};
use anyhow::{Context, Result};
use std::path::Path;
use turso::Connection;

pub(crate) struct MemorySnapshot {
    pub image: Image,
}
pub(crate) struct Image {
    tables: Vec<Table>,
    indexes: Vec<String>,
}
struct Table {
    schema: String,
    name: String,
    columns: Vec<String>,
    rows: Vec<Vec<SqlValue>>,
}
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
impl Image {
    pub(crate) async fn capture(conn: &Connection) -> Result<Self> {
        let schema = actor::query(conn, "SELECT name,sql FROM sqlite_schema WHERE type='table' AND substr(name,1,16) <> '__turso_internal' AND substr(name,1,7) <> 'sqlite_' ORDER BY name", ()).await?;
        let mut tables = Vec::new();
        for entry in schema.rows {
            let name: String = entry.get(0)?;
            if name.starts_with("sqlite_") {
                continue;
            }
            let data = actor::query(conn, &format!("SELECT rowid,* FROM {} ORDER BY rowid", quote(&name)), ()).await?;
            let mut rows = Vec::new();
            for row in data.rows {
                let mut values = Vec::new();
                for index in 0..row.column_count() {
                    values.push(SqlValue::capture(row.get_value(index)?));
                }
                rows.push(values);
            }
            tables.push(Table { schema: entry.get(1)?, name, columns: data.columns, rows });
        }
        let objects = actor::query(conn, "SELECT sql FROM sqlite_schema WHERE type IN ('index','trigger','view') AND sql IS NOT NULL AND substr(name,1,16) <> '__turso_internal' AND substr(name,1,7) <> 'sqlite_'", ())
            .await?;
        let indexes = objects.rows.iter().map(|row| row.get::<String>(0)).collect::<turso::Result<Vec<_>>>()?;
        Ok(Self { tables, indexes })
    }
    pub(crate) async fn restore(&self) -> Result<Connection> {
        let mut conn = actor::connect(Path::new(":memory:"), crate::Io::Memory).await?;
        actor::query(&conn, "PRAGMA capture_data_changes_conn = 'off'", ()).await?;
        let tx = conn.transaction().await?;
        for table in &self.tables {
            if table.name.starts_with("turso_cdc") {
                tx.execute(format!("DELETE FROM {}", quote(&table.name)), ()).await?;
            } else {
                tx.execute(&table.schema, ()).await?;
            }
            // The first selected column is the physical identity even for INTEGER PRIMARY KEY tables.
            let columns = std::iter::once("rowid".to_owned())
                .chain(table.columns.iter().skip(1).map(|name| quote(name)))
                .collect::<Vec<_>>()
                .join(",");
            let placeholders = vec!["?"; table.columns.len()].join(",");
            let sql = format!("INSERT INTO {} ({columns}) VALUES ({placeholders})", quote(&table.name));
            for row in &table.rows {
                tx.execute(&sql, row.iter().map(SqlValue::value).collect::<Vec<_>>()).await?;
            }
        }
        for sql in &self.indexes {
            tx.execute(sql, ()).await?;
        }
        tx.commit().await?;
        actor::query(&conn, "PRAGMA capture_data_changes_conn = 'full'", ()).await?;
        Ok(conn)
    }
}
impl Node {
    pub(crate) async fn schema_fingerprint(conn: &Connection) -> Result<String> {
        super::memo::rows_hash(conn, "PRAGMA schema_version", ()).await
    }
    pub(crate) async fn snapshot_schema_change(&self, conn: &Connection, id: &str, before: &str) -> Result<()> {
        let pending = actor::query(conn, "SELECT value FROM meta WHERE key='schema_snapshot_pending'", ()).await?;
        if pending.rows.is_empty() && Self::schema_fingerprint(conn).await? == before {
            return Ok(());
        }
        let seq = actor::cursor(conn).await?;
        if actor::meta(conn, "commit_epoch").await? != actor::meta(conn, &format!("boundary:{seq}")).await? {
            // A selective receive cut is not a historical snapshot boundary. The
            // committed pending marker remains until a later turn closes the gap.
            return Ok(());
        }
        let generation: i64 = actor::meta(conn, "generation").await?.parse()?;
        let revision = actor::code(conn).await?.revision;
        // A schema change is a code-revision boundary, not a message boundary: the image below is the
        // base for CDC replay (invariant 17) and is recorded in meta, never in `snapshots`, which fork,
        // memo and validation read as "state after N messages under the code of that time".
        let base = if !self.is_memory(id)? {
            let path = self.snapshot_path(id, generation, seq).with_extension(format!("code.{revision}.db"));
            actor::write_snapshot_file(conn, &path).await?;
            path.to_str().context("snapshot path is not UTF-8")?.to_owned()
        } else {
            actor::compact_cdc(conn).await?;
            let key = format!("memory:{id}:{generation}:{seq}:code:{revision}");
            let image = Image::capture(conn).await?;
            self.memory_snapshots.lock().await.insert(key.clone(), MemorySnapshot { image });
            key
        };
        actor::set_meta(conn, "cdc_base", &base).await?;
        conn.execute("DELETE FROM meta WHERE key='schema_snapshot_pending'", ()).await?;
        Ok(())
    }
    pub(crate) async fn forget_missing_index(&self) -> Result<()> {
        let live = self.actor_ids()?;
        let mut names = self.names.lock().await;
        let conn = crate::directory::connection(&mut names, &self.dir, self.config.io).await?;
        let rows = actor::query(conn, "SELECT id FROM names UNION SELECT id FROM groups UNION SELECT id FROM who_runs", ()).await?;
        for row in rows.rows {
            let id: String = row.get(0)?;
            if live.contains(&id)
                || match &self.remote {
                    Some(store) => store.has_snapshot(&id).await?,
                    None => false,
                }
            {
                continue;
            }
            let tx = conn.transaction().await?;
            for table in ["names", "groups", "who_runs"] {
                tx.execute(format!("DELETE FROM {table} WHERE id=?"), [id.as_str()]).await?;
            }
            tx.commit().await?;
        }
        Ok(())
    }
    pub(crate) fn is_memory(&self, id: &str) -> Result<bool> {
        Ok(self.memory_ids.lock().map_err(|_| anyhow::anyhow!("memory actor registry poisoned"))?.iter().any(|known| known == id))
    }
    pub(crate) async fn snapshot_actor(&self, conn: &Connection, id: &str, seq: i64) -> Result<()> {
        if !self.is_memory(id)? {
            return actor::snapshot(conn, &self.snapshot_path(id, actor::meta(conn, "generation").await?.parse()?, seq), seq).await;
        }
        let generation: i64 = actor::meta(conn, "generation").await?.parse()?;
        let key = format!("memory:{id}:{generation}:{seq}");
        if self.memory_snapshots.lock().await.contains_key(&key) {
            return Ok(());
        }
        actor::compact_cdc(conn).await?;
        let image = Image::capture(conn).await?;
        self.memory_snapshots.lock().await.insert(key.clone(), MemorySnapshot { image });
        conn.execute("INSERT OR IGNORE INTO snapshots(seq,path) VALUES (?,?)", turso::params![seq, key]).await?;
        Ok(())
    }
    pub(crate) async fn snapshot_connection(&self, path: &str) -> Result<Connection> {
        if path.starts_with("memory:") {
            let snapshots = self.memory_snapshots.lock().await;
            let snapshot = snapshots.get(path).context("ephemeral snapshot no longer exists")?;
            return snapshot.image.restore().await;
        }
        // Replay images are immutable. The live-actor opener performs migration
        // writes; even ignored writes can append CDC transaction markers and
        // advance this image's MAX(change_id), hiding source rows on later replay.
        let source = actor::connect_reader(Path::new(path), self.config.io).await?;
        Image::capture(&source).await?.restore().await
    }
}
