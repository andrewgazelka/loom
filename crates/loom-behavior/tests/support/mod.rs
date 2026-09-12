use anyhow::{Context, Result, ensure};
use loom_actor::{Actor, Config, DefaultEffects, EffectHandler, Node, Registry};
use loom_store::Store;
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc};
use tokio::{process::Command, sync::OnceCell};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureHashes {
    handler: String,
    promoted: String,
    no_schema: String,
}

pub struct Fixtures {
    pub store: Store,
    pub handler: String,
    pub promoted: String,
    pub no_schema: String,
    log_rows: i64,
    _directory: tempfile::TempDir,
}

static FIXTURES: OnceCell<Result<Fixtures, String>> = OnceCell::const_new();

pub async fn fixtures() -> &'static Fixtures {
    FIXTURES
        .get_or_init(|| async { build().await.map_err(|error| format!("{error:#}")) })
        .await
        .as_ref()
        .unwrap_or_else(|error| panic!("Rust guest fixture build failed: {error}"))
}

async fn build() -> Result<Fixtures> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("definitions.sqlite");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("crate parent")?
        .parent()
        .context("workspace root")?
        .to_owned();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .current_dir(root)
        .args([
            "run",
            "--quiet",
            "-p",
            "loom-behavior",
            "--example",
            "loom-behavior-fixtures",
            "--",
        ])
        .arg(&database)
        .kill_on_drop(true);
    let invocation = format!("{command:?}");
    let output = command
        .output()
        .await
        .with_context(|| format!("run fixture builder: {invocation}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    ensure!(
        output.status.success(),
        "fixture builder {invocation} exited {}\nstderr:\n{stderr}\nstdout:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout)
    );
    let hashes: FixtureHashes = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "fixture builder {invocation} returned invalid JSON\nstderr:\n{stderr}\nstdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })?;
    for hash in [&hashes.handler, &hashes.promoted, &hashes.no_schema] {
        ensure!(
            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "fixture builder {invocation} returned invalid definition hash {hash:?}\nstderr:\n{stderr}"
        );
    }
    let store = Store::open(database)?;
    let log_rows = store.with_connection(|connection| {
        Ok(connection.query_row("SELECT count(*) FROM log", [], |row| row.get(0))?)
    })?;
    Ok(Fixtures {
        store,
        handler: hashes.handler,
        promoted: hashes.promoted,
        no_schema: hashes.no_schema,
        log_rows,
        _directory: directory,
    })
}

impl Fixtures {
    pub async fn node(&self, directory: &std::path::Path) -> Node {
        self.node_with_effects(directory, Arc::new(DefaultEffects))
            .await
    }

    pub async fn node_with_effects(
        &self,
        directory: &std::path::Path,
        effects: Arc<dyn EffectHandler>,
    ) -> Node {
        let mut registry = Registry::new();
        for hash in [&self.handler, &self.promoted] {
            let behavior = loom_behavior::register(&mut registry, self.store.clone(), hash)
                .await
                .unwrap();
            assert_eq!(loom_actor::Behavior::hash(behavior.as_ref()), hash);
            assert!(!loom_actor::Behavior::schema(behavior.as_ref()).is_empty());
            assert!(registry.contains_key(hash));
        }
        Node::new(directory, registry, effects, Config::default())
            .await
            .unwrap()
    }

    pub fn assert_no_legacy_execution(&self) {
        self.store
            .with_connection(|connection| {
                for table in ["actors", "inbox"] {
                    let count: i64 = connection.query_row(
                        &format!("SELECT count(*) FROM {table}"),
                        [],
                        |row| row.get(0),
                    )?;
                    assert_eq!(count, 0, "legacy {table} was used");
                }
                let count: i64 =
                    connection.query_row("SELECT count(*) FROM log", [], |row| row.get(0))?;
                assert_eq!(count, self.log_rows, "execution appended to the legacy log");
                Ok(())
            })
            .unwrap();
    }
}

pub async fn integer(actor: &Actor, sql: &str) -> i64 {
    let rows = actor.sql(sql, ()).await.unwrap();
    assert_eq!(rows.rows.len(), 1);
    rows.rows[0].get(0).unwrap()
}
