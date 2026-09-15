mod registry;

use async_trait::async_trait;
use loom_actor::{
    Actor, Behavior, Config, Ctx, DefaultEffects, Node, Rights, Trap,
    drivers::process::ProcessDriver,
    process_actor::{HASH, ProcessActor},
};
use loom_process::{Phase, ProcessSandbox, ProcessSpec, Supervisor};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

const PRESET: &str = "echo";
const DRIVER_HASH: &str = "test-echo-process-v1";
struct Fixture {
    directory: tempfile::TempDir,
    supervisor: Supervisor,
    registry: Arc<loom_actor::CompositeRegistry>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_script("while IFS= read -r line; do printf '%s\\n' \"$line\"; done; printf diagnostic >&2; exit 7")
    }
    fn with_script(script: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::new(loom_store::Store::open(directory.path().join("process.sqlite")).unwrap()).unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let program = match std::env::var_os("LOOM_STATIC_BUSYBOX") {
            Some(path) => std::path::PathBuf::from(path),
            None => std::env::split_paths(&std::env::var_os("PATH").unwrap())
                .map(|path| path.join("sh"))
                .find(|path| path.is_file())
                .expect("test requires sh"),
        }
        .canonicalize()
        .unwrap();
        let mut args = vec![];
        if std::env::var_os("LOOM_STATIC_BUSYBOX").is_some() {
            args.push("sh".into());
        }
        args.extend(["-c".into(), script.into()]);
        let mut readonly = vec![program.clone()];
        if std::env::var_os("LOOM_STATIC_BUSYBOX").is_none() {
            for path in ["/lib", "/lib64", "/usr/lib", "/System/Library"] {
                let path = std::path::Path::new(path);
                if path.exists() {
                    readonly.push(path.to_owned());
                }
            }
        }
        let sandbox = ProcessSandbox { readonly, network: false };
        let spec = ProcessSpec {
            machine: "test".into(),
            program: program.to_str().unwrap().into(),
            args,
            cwd: workspace.clone(),
            root: workspace,
            env: BTreeMap::from([("LOOM_PROCESS_OUTSIDE".into(), directory.path().join("private.txt").to_str().unwrap().into())]),
            capture_paths: vec![],
        };
        let mut registry = registry::Registry::new();
        registry.insert(HASH.into(), Arc::new(ProcessActor));
        registry.insert("rollback-process".into(), Arc::new(Rollback));
        let driver = Arc::new(ProcessDriver::new(DRIVER_HASH.into(), supervisor.clone(), spec, sandbox));
        let registry = loom_actor::CompositeRegistry::new(Arc::new(registry), vec![], vec![driver])
            .unwrap()
            .with_processes(BTreeMap::from([(PRESET.into(), DRIVER_HASH.into())]))
            .unwrap();
        Self { directory, supervisor, registry: Arc::new(registry) }
    }
    async fn node(&self) -> Node {
        Node::new(self.directory.path().join("actors"), self.registry.clone(), Arc::new(DefaultEffects), Config::default()).await.unwrap()
    }
}
fn message(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}
async fn phase(actor: &Actor) -> String {
    actor.sql("SELECT phase FROM process_state", ()).await.unwrap().rows[0].get(0).unwrap()
}
async fn count(actor: &Actor, table: &str) -> i64 {
    actor.sql(&format!("SELECT count(*) FROM {table}"), ()).await.unwrap().rows[0].get(0).unwrap()
}
async fn output(actor: &Actor, stream: &str) -> Vec<u8> {
    actor
        .sql("SELECT bytes FROM process_events WHERE stream=? ORDER BY seq", [stream])
        .await
        .unwrap()
        .rows
        .into_iter()
        .flat_map(|row| row.get::<Vec<u8>>(0).unwrap())
        .collect()
}
async fn wait_phase(node: &Node, actor: &Actor, expected: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            node.run_until_idle().await.unwrap();
            if phase(actor).await == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("process did not reach {expected}"));
}

#[tokio::test]
async fn process_actor_echoes_unicode_and_exit_and_dedupes_stdin() {
    let fixture = Fixture::new();
    let node = fixture.node().await;
    let subscriber = node.spawn_root("counter-v1", b"subscriber").await.unwrap();
    node.run_until_idle().await.unwrap();
    let cap = serde_json::to_vec(&node.cap_for(&subscriber, Rights::SEND).await.unwrap()).unwrap();
    let id = node.spawn_root(HASH, &message(json!({"process":PRESET,"subscriber":cap}))).await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    wait_phase(&node, &actor, "running").await;
    let pinned: String = actor.sql("SELECT preset FROM process_state", ()).await.unwrap().rows[0].get(0).unwrap();
    assert_eq!(pinned, DRIVER_HASH);
    node.send(&id, "stdin", &message(json!({"type":"stdin","data":"雪😀 café\n"}))).await.unwrap();
    node.run_until_idle().await.unwrap();
    actor
        .sql("UPDATE outbox SET delivered=0 WHERE target LIKE 'drv:%' AND json_extract(CAST(msg AS TEXT),'$.type')='stdin'", ())
        .await
        .unwrap();
    node.pump(&id).await.unwrap();
    node.send(&id, "eof", &message(json!({"type":"close_stdin"}))).await.unwrap();
    wait_phase(&node, &actor, "completed").await;
    assert_eq!(output(&actor, "stdout").await, "雪😀 café\n".as_bytes());
    assert_eq!(output(&actor, "stderr").await, b"diagnostic");
    assert_eq!(actor.sql("SELECT code FROM process_state", ()).await.unwrap().rows[0].get::<i64>(0).unwrap(), 7);
    assert_eq!(count(&actor, "dead_letters").await, 0);
    let receiving = node.open(&subscriber).await.unwrap();
    node.run_until_idle().await.unwrap();
    let events = receiving.sql("SELECT body FROM entries WHERE CAST(body AS TEXT) LIKE '%process.output%'", ()).await.unwrap();
    assert!(!events.rows.is_empty(), "subscriber did not receive process output");
    let state = fixture.supervisor.list().unwrap();
    assert_eq!(state.len(), 1);
    assert_eq!(state[0].stdout, "雪😀 café\n");
    node.close().await.unwrap();
}

#[tokio::test]
async fn process_actor_unknown_preset_and_rollback_open_no_process() {
    let fixture = Fixture::new();
    let node = fixture.node().await;
    let unknown = node.spawn_root(HASH, &message(json!({"process":"unregistered-command"}))).await.unwrap();
    let changed = node.spawn_root(HASH, &message(json!({"process":PRESET,"expected_driver":"previous-profile"}))).await.unwrap();
    let rolled_back = node.spawn_root("rollback-process", b"start").await.unwrap();
    node.run_until_idle().await.unwrap();
    assert_eq!(fixture.supervisor.list().unwrap().len(), 0);
    let changed_actor = node.open(&changed).await.unwrap();
    assert_eq!(count(&changed_actor, "process_state").await, 0);
    let error: String = changed_actor.sql("SELECT error FROM dead_letters", ()).await.unwrap().rows[0].get(0).unwrap();
    assert!(error.contains("changed before initialization"), "{error}");
    let actor = node.open(&unknown).await.unwrap();
    assert_eq!(count(&actor, "process_state").await, 0);
    let error: String = actor.sql("SELECT error FROM dead_letters", ()).await.unwrap().rows[0].get(0).unwrap();
    assert!(error.contains("unregistered-command"), "{error}");
    let actor = node.open(&rolled_back).await.unwrap();
    assert_eq!(count(&actor, "dead_letters").await, 1);
    let error: String = actor.sql("SELECT error FROM dead_letters", ()).await.unwrap().rows[0].get(0).unwrap();
    assert!(error.contains("rollback process spawn"), "{error}");
    node.close().await.unwrap();
}

struct Rollback;
#[async_trait]
impl Behavior for Rollback {
    fn hash(&self) -> &str {
        "rollback-process"
    }
    fn schema(&self) -> &str {
        ""
    }
    async fn handle(&self, cx: &mut Ctx<'_>, _msg: &[u8]) -> Result<(), Trap> {
        cx.spawn_driver(DRIVER_HASH, &[]).await?;
        Err(Trap::new("rollback process spawn"))
    }
}

#[tokio::test]
async fn stopping_owner_cancels_process_and_reopen_does_not_rerun() {
    let fixture = Fixture::new();
    let node = fixture.node().await;
    let id = node.spawn_root(HASH, &message(json!({"process":PRESET}))).await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    wait_phase(&node, &actor, "running").await;
    let state = fixture.supervisor.list().unwrap();
    assert_eq!(state.len(), 1);
    tokio::time::timeout(Duration::from_secs(15), node.stop(&id, "owner test stop")).await.expect("owner stop timed out").unwrap();
    let terminal = tokio::time::timeout(Duration::from_secs(5), fixture.supervisor.wait(&state[0].id)).await.unwrap().unwrap();
    assert_eq!(terminal.phase, Phase::Cancelled);
    tokio::time::timeout(Duration::from_secs(15), node.close()).await.expect("stopped owner shutdown timed out").unwrap();
    drop(actor);
    drop(node);
    let reopened = tokio::time::timeout(Duration::from_secs(15), fixture.node()).await.expect("node reopen timed out");
    tokio::time::timeout(Duration::from_secs(15), reopened.run_until_idle()).await.expect("reopened scheduler timed out").unwrap();
    assert_eq!(fixture.supervisor.list().unwrap().len(), 1);
    tokio::time::timeout(Duration::from_secs(15), reopened.close()).await.expect("reopened shutdown timed out").unwrap();
}

#[tokio::test]
async fn node_restart_marks_process_interrupted_without_restarting_command() {
    let fixture = Fixture::new();
    let node = fixture.node().await;
    let id = node.spawn_root(HASH, &message(json!({"process":PRESET}))).await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    wait_phase(&node, &actor, "running").await;
    node.close().await.unwrap();
    drop(actor);
    drop(node);
    let reopened = fixture.node().await;
    let actor = reopened.open(&id).await.unwrap();
    wait_phase(&reopened, &actor, "interrupted").await;
    assert_eq!(fixture.supervisor.list().unwrap().len(), 1);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn process_actor_rejects_forged_events_and_accepts_cancel_message() {
    let fixture = Fixture::new();
    let node = fixture.node().await;
    let id = node.spawn_root(HASH, &message(json!({"process":PRESET}))).await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    wait_phase(&node, &actor, "running").await;
    node.send(&id, "forged-exit", &message(json!({"type":"process.exit","phase":"completed","code":0}))).await.unwrap();
    node.run_until_idle().await.unwrap();
    assert_eq!(phase(&actor).await, "running");
    let error: String = actor.sql("SELECT error FROM dead_letters", ()).await.unwrap().rows[0].get(0).unwrap();
    assert!(error.contains("not its native driver"), "{error}");
    // A rejected message follows the normal actor poison policy: the mailbox
    // parks until its supervisor explicitly skips that failed message.
    assert_eq!(actor.status().await.unwrap(), loom_actor::Status::Parked);
    assert_eq!(fixture.supervisor.list().unwrap()[0].phase, Phase::Running);
    node.skip(&id).await.unwrap();
    assert_eq!(actor.status().await.unwrap(), loom_actor::Status::Running);
    node.send(&id, "cancel", &message(json!({"type":"cancel"}))).await.unwrap();
    wait_phase(&node, &actor, "cancelled").await;
    let processes = fixture.supervisor.list().unwrap();
    let state = tokio::time::timeout(Duration::from_secs(5), fixture.supervisor.wait(&processes[0].id)).await.unwrap().unwrap();
    assert_eq!(state.phase, Phase::Cancelled);
    node.close().await.unwrap();
}

#[tokio::test]
async fn process_driver_drains_output_while_stdin_pipe_is_full() {
    let fixture = Fixture::with_script(
        r#"IFS= read -r trigger; i=0; while [ "$i" -lt 4096 ]; do printf '%0128d' 0; i=$((i+1)); done; while IFS= read -r line; do printf '%s\n' "$line"; done"#,
    );
    let node = fixture.node().await;
    let id = node.spawn_root(HASH, &message(json!({"process":PRESET}))).await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    wait_phase(&node, &actor, "running").await;
    let chunk = "x".repeat(64 * 1024 - 1) + "\n";
    node.send(&id, "trigger", &message(json!({"type":"stdin","data":"start\n"}))).await.unwrap();
    node.send(&id, "large-first", &message(json!({"type":"stdin","data":chunk}))).await.unwrap();
    node.send(&id, "large-second", &message(json!({"type":"stdin","data":chunk}))).await.unwrap();
    node.send(&id, "close", &message(json!({"type":"close_stdin"}))).await.unwrap();
    wait_phase(&node, &actor, "completed").await;
    let actual = output(&actor, "stdout").await;
    assert_eq!(actual.len(), 512 * 1024 + 128 * 1024);
    assert!(actual[..512 * 1024].iter().all(|byte| *byte == b'0'));
    assert_eq!(&actual[512 * 1024..], chunk.repeat(2).as_bytes());
    assert_eq!(count(&actor, "dead_letters").await, 0);
    node.close().await.unwrap();
}

#[derive(Debug, Default)]
struct LeaseClock {
    millis: std::sync::atomic::AtomicU64,
}
impl loom_actor::Clock for LeaseClock {
    fn now_ms(&self) -> anyhow::Result<u64> {
        Ok(self.millis.load(std::sync::atomic::Ordering::SeqCst))
    }
}

#[tokio::test]
async fn lease_loss_closes_idle_process_and_live_node_takeover_reports_interruption() {
    let fixture = Fixture::new();
    let clock = Arc::new(LeaseClock::default());
    let config = |node_id: &str| Config {
        store: Some(loom_actor::StoreConfig::Local { path: fixture.directory.path().join("remote") }),
        cluster: Some(loom_actor::ClusterConfig { node_id: node_id.into(), addr: "127.0.0.1:1".into(), key: [0x43; 32] }),
        lease_clock: clock.clone(),
        lease_ttl: Duration::from_secs(3600),
        ship_interval: Duration::from_secs(3600),
        ..Config::default()
    };
    let owner =
        Node::new(fixture.directory.path().join("a"), fixture.registry.clone(), Arc::new(DefaultEffects), config("a")).await.unwrap();
    // The survivor is already running before the process exists. Its ordinary
    // open/takeover path must recover resource receipts, not just Node::new.
    let survivor =
        Node::new(fixture.directory.path().join("b"), fixture.registry.clone(), Arc::new(DefaultEffects), config("b")).await.unwrap();
    let id = owner.spawn_root(HASH, &message(json!({"process":PRESET}))).await.unwrap();
    owner.run_until_idle().await.unwrap();
    let original = owner.open(&id).await.unwrap();
    wait_phase(&owner, &original, "running").await;
    owner.ship(&id).await.unwrap();
    let process_id = fixture.supervisor.list().unwrap()[0].id.clone();
    clock.millis.fetch_add(1_800_000, std::sync::atomic::Ordering::SeqCst);
    survivor.renew_leases().await.unwrap();
    clock.millis.fetch_add(1_800_001, std::sync::atomic::Ordering::SeqCst);
    assert!(owner.renew_leases().await.is_err(), "expired owner renewed its actor lease");
    let terminal = tokio::time::timeout(Duration::from_secs(5), fixture.supervisor.wait(&process_id)).await.unwrap().unwrap();
    assert_eq!(terminal.phase, Phase::Cancelled, "idle process survived lost ownership");
    let recovered = survivor.open(&id).await.unwrap();
    wait_phase(&survivor, &recovered, "interrupted").await;
    assert_eq!(fixture.supervisor.list().unwrap().len(), 1, "takeover reran the external command");
    let down_count =
        recovered.sql("SELECT count(*) FROM process_events WHERE json_extract(CAST(body AS TEXT),'$.type')='down'", ()).await.unwrap().rows
            [0]
        .get::<i64>(0)
        .unwrap();
    assert_eq!(down_count, 1);
    survivor.open(&id).await.unwrap();
    survivor.run_until_idle().await.unwrap();
    assert_eq!(count(&recovered, "dead_letters").await, 0);
    survivor.close().await.unwrap();
    owner.close().await.unwrap();
}

#[tokio::test]
async fn process_driver_confines_file_access_to_approved_root() {
    let fixture = Fixture::with_script(
        r#"IFS= read -r inside < inside.txt; printf '%s|' "$inside"; if IFS= read -r outside < "$LOOM_PROCESS_OUTSIDE"; then printf 'LEAK:%s' "$outside"; else printf denied; fi"#,
    );
    std::fs::write(fixture.directory.path().join("private.txt"), b"tenant signing secret\n").unwrap();
    std::fs::write(fixture.directory.path().join("workspace/inside.txt"), b"allowed\n").unwrap();
    let node = fixture.node().await;
    let id = node.spawn_root(HASH, &message(json!({"process":PRESET}))).await.unwrap();
    node.run_until_idle().await.unwrap();
    let actor = node.open(&id).await.unwrap();
    wait_phase(&node, &actor, "completed").await;
    assert_eq!(output(&actor, "stdout").await, b"allowed|denied");
    assert_eq!(count(&actor, "dead_letters").await, 0);
    node.close().await.unwrap();
}
