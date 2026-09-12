//! Canonical runtime history images. Revisions count publications, not contiguous
//! inbox cursors: selective receive can commit while the cursor remains zero.
use crate::{Node, actor, effects::ReplayEffects, history::ReplayMode};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use turso::{Connection, Value};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum Cell {
    Null,
    Integer(i64),
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}
impl Cell {
    fn capture(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Integer(value) => Self::Integer(value),
            Value::Real(value) => Self::Real(value.to_bits()),
            Value::Text(value) => Self::Text(value),
            Value::Blob(value) => Self::Blob(value),
        }
    }
    fn value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Integer(value) => Value::Integer(*value),
            Self::Real(value) => Value::Real(f64::from_bits(*value)),
            Self::Text(value) => Value::Text(value.clone()),
            Self::Blob(value) => Value::Blob(value.clone()),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct TableImage {
    name: String,
    columns: Vec<String>,
    rows: Vec<Vec<Cell>>,
    removed: Vec<Vec<Cell>>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Segment {
    pub(crate) from_seq: i64,
    pub(crate) to_seq: i64,
    tables: Vec<TableImage>,
}
impl Segment {
    pub(crate) async fn capture(conn: &Connection, from_seq: i64, to_seq: i64) -> Result<Self> {
        ensure!(from_seq >= 0 && to_seq >= from_seq, "invalid history publication window {from_seq}..{to_seq}");
        let mut names = crate::schema::SYSTEM_TABLES.to_vec();
        // Restart intensity is runtime state even though historical domain hashing
        // retains its existing treatment of this table.
        names.push("restarts");
        names.sort_unstable();
        names.dedup();
        let mut tables = Vec::new();
        for name in names {
            if name == "snapshots" {
                continue;
            }
            let result = actor::query(conn, &format!("SELECT * FROM {} ORDER BY rowid", quote(name)), ()).await?;
            let mut rows = Vec::new();
            for row in result.rows {
                let mut cells = Vec::new();
                for column in 0..row.column_count() {
                    cells.push(Cell::capture(row.get_value(column)?));
                }
                rows.push(cells);
            }
            // Canonical ordering is independent of physical row insertion order.
            rows.sort_by_cached_key(|row| serde_json::to_vec(row).expect("Cell serialization is infallible"));
            tables.push(TableImage { name: name.to_owned(), columns: result.columns, rows, removed: Vec::new() });
        }
        Ok(Self { from_seq, to_seq, tables })
    }
    pub(crate) fn delta_from(&self, previous: &Self) -> Result<Self> {
        ensure!(self.tables.len() == previous.tables.len(), "history baseline table count differs");
        let mut tables = Vec::new();
        for (table, baseline) in self.tables.iter().zip(&previous.tables) {
            ensure!(table.name == baseline.name && table.columns == baseline.columns, "history baseline schema differs for {}", table.name);
            ensure!(table.removed.is_empty() && baseline.removed.is_empty(), "history delta requires full runtime images");
            tables.push(TableImage {
                name: table.name.clone(),
                columns: table.columns.clone(),
                rows: difference(&table.rows, &baseline.rows)?,
                removed: difference(&baseline.rows, &table.rows)?,
            });
        }
        Ok(Self { from_seq: self.from_seq, to_seq: self.to_seq, tables })
    }
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let segment: Self = serde_json::from_slice(bytes).context("decode history segment")?;
        ensure!(segment.from_seq >= 0 && segment.to_seq >= segment.from_seq, "invalid history segment publication window");
        Ok(segment)
    }
    async fn apply(&self, conn: &mut Connection) -> Result<()> {
        self.apply_image(conn, false).await
    }
    async fn apply_full(&self, conn: &mut Connection) -> Result<()> {
        self.apply_image(conn, true).await
    }
    async fn apply_image(&self, conn: &mut Connection, full: bool) -> Result<()> {
        let tx = conn.transaction().await?;
        let mut seen = std::collections::BTreeSet::new();
        for table in &self.tables {
            ensure!(
                (crate::schema::SYSTEM_TABLES.contains(&table.name.as_str()) || table.name == "restarts") && table.name != "snapshots",
                "history segment names non-runtime table {}",
                table.name
            );
            ensure!(seen.insert(table.name.as_str()), "history segment repeats table {}", table.name);
            let actual = actor::query(&tx, &format!("SELECT * FROM {} LIMIT 0", quote(&table.name)), ()).await?;
            ensure!(actual.columns == table.columns, "history segment schema differs for {}", table.name);
            if full {
                ensure!(table.removed.is_empty(), "full history image contains removals for {}", table.name);
                tx.execute(format!("DELETE FROM {}", quote(&table.name)), ()).await?;
            } else {
                let predicate = table.columns.iter().map(|column| format!("{} IS ?", quote(column))).collect::<Vec<_>>().join(" AND ");
                let delete = format!(
                    "DELETE FROM {name} WHERE rowid IN (SELECT rowid FROM {name} WHERE {predicate} LIMIT 1)",
                    name = quote(&table.name)
                );
                for row in &table.removed {
                    ensure!(row.len() == table.columns.len(), "history removal row width differs for {}", table.name);
                    let changed = tx.execute(&delete, row.iter().map(Cell::value).collect::<Vec<_>>()).await?;
                    ensure!(changed == 1, "history removal did not match one row in {}", table.name);
                }
            }
            let columns = table.columns.iter().map(|name| quote(name)).collect::<Vec<_>>().join(",");
            let placeholders = vec!["?"; table.columns.len()].join(",");
            let sql = format!("INSERT INTO {} ({columns}) VALUES ({placeholders})", quote(&table.name));
            for row in &table.rows {
                ensure!(row.len() == table.columns.len(), "history segment row width differs for {}", table.name);
                tx.execute(&sql, row.iter().map(Cell::value).collect::<Vec<_>>()).await?;
            }
        }
        for name in crate::schema::SYSTEM_TABLES.iter().copied().chain(std::iter::once("restarts")) {
            ensure!(name == "snapshots" || seen.contains(name), "history segment omits runtime table {name}");
        }
        tx.commit().await?;
        Ok(())
    }
}
struct RowCount {
    row: Vec<Cell>,
    count: usize,
}
fn row_counts(rows: &[Vec<Cell>]) -> Result<std::collections::BTreeMap<Vec<u8>, RowCount>> {
    let mut counts = std::collections::BTreeMap::new();
    for row in rows {
        let entry = counts.entry(serde_json::to_vec(row)?).or_insert_with(|| RowCount { row: row.clone(), count: 0 });
        entry.count += 1;
    }
    Ok(counts)
}
fn difference(rows: &[Vec<Cell>], baseline: &[Vec<Cell>]) -> Result<Vec<Vec<Cell>>> {
    let counts = row_counts(rows)?;
    let baseline_counts = row_counts(baseline)?;
    let mut difference = Vec::new();
    for (key, entry) in counts {
        let previous = baseline_counts.get(&key).map_or(0, |entry| entry.count);
        difference.extend(std::iter::repeat_n(entry.row, entry.count.saturating_sub(previous)));
    }
    Ok(difference)
}
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

struct Staging {
    paths: Vec<PathBuf>,
}
impl Staging {
    fn track(&mut self, path: PathBuf) {
        self.paths.push(PathBuf::from(format!("{}-wal", path.display())));
        self.paths.push(PathBuf::from(format!("{}-shm", path.display())));
        self.paths.push(path);
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = std::fs::remove_file(path);
        }
    }
}
impl Node {
    pub(crate) async fn restore_history(
        &self,
        snapshot: &[u8],
        segments: &[Vec<u8>],
        id: &str,
        destination: &Path,
        snapshot_revision: i64,
        head_seq: i64,
    ) -> Result<()> {
        let suffix = crate::ids::root();
        let source_path = destination.with_extension(format!("{suffix}.source"));
        let target_path = destination.with_extension(format!("{suffix}.replay"));
        let ready = destination.with_extension(format!("{suffix}.ready"));
        let base = destination.with_extension("remote-base.db");
        let mut cleanup = Staging { paths: Vec::new() };
        for path in [&source_path, &target_path, &ready] {
            cleanup.track(path.clone());
        }
        std::fs::write(&source_path, snapshot)?;
        std::fs::write(&target_path, snapshot)?;
        let mut source = actor::connect(&source_path, self.config.io).await?;
        let mut target = actor::connect(&target_path, self.config.io).await?;
        ensure!(actor::meta(&source, "id").await? == id, "actor {id}: snapshot identity mismatch");
        ensure!(
            actor::meta(&source, "durability_seq").await?.parse::<i64>()? == snapshot_revision,
            "actor {id}: snapshot revision differs from head"
        );
        let snapshot_cursor = actor::cursor(&target).await?;
        let mut previous = snapshot_revision;
        for bytes in segments {
            let segment = Segment::decode(bytes)?;
            let expected = previous.checked_add(1).context("history revision overflow")?;
            ensure!(
                segment.from_seq == expected && segment.to_seq <= head_seq,
                "actor {id}: history segment publication gap: expected from_seq {expected}, segment from_seq {} to_seq {}, head.seq {head_seq}",
                segment.from_seq,
                segment.to_seq
            );
            previous = segment.to_seq;
            segment.apply(&mut source).await?;
        }
        ensure!(actor::meta(&source, "id").await? == id, "actor {id}: segment identity mismatch");
        actor::set_meta(&target, "replay_source", id).await?;
        let inbox = actor::query(&source, "SELECT seq,key,sender,msg,received_at FROM inbox ORDER BY seq", ()).await?;
        for row in inbox.rows {
            let values = (0..row.column_count()).map(|column| row.get_value(column)).collect::<turso::Result<Vec<_>>>()?;
            target.execute("INSERT OR IGNORE INTO inbox(seq,key,sender,msg,received_at) VALUES (?,?,?,?,?)", values).await?;
        }
        let effects = ReplayEffects::load(&source).await?;
        let cursor = actor::cursor(&source).await?;
        if let Some(verdict) = self.replay(&source, &mut target, id, cursor, &effects, ReplayMode::Remote).await? {
            anyhow::bail!("actor {id}: remote history replay failed: {verdict:?}");
        }
        Segment::capture(&source, 0, previous).await?.apply_full(&mut target).await?;
        target.execute("DELETE FROM snapshots", ()).await?;
        target
            .execute(
                "INSERT INTO snapshots(seq,path) VALUES (?,?)",
                turso::params![snapshot_cursor, base.to_str().context("non-UTF8 remote snapshot path")?],
            )
            .await?;
        target.execute(format!("VACUUM INTO '{}'", ready.to_str().context("non-UTF8 restore path")?.replace('\'', "''")), ()).await?;
        drop(target);
        drop(source);
        std::fs::write(&base, snapshot)?;
        std::fs::rename(&ready, destination)?;
        Ok(())
    }
}
