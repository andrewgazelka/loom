#![cfg(target_os = "linux")]
mod registry;

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use loom_actor::{
    Actor, Config, DefaultEffects, Node,
    drivers::vm::{VmConfig, VmDriver, VmImageDestination, VmImages},
    vm_actor::{HASH, VmActor},
};
use loom_process::{Phase, Supervisor};
use loom_proto::{CasReference, VmNetwork, VmSpec};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn image_reference(byte: &str) -> CasReference {
    CasReference { reference: loom_proto::cid_for_hash(&byte.repeat(32), loom_proto::DAG_CBOR_CODEC).unwrap() }
}
fn required_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("native VM tests require {name}"))).canonicalize().unwrap()
}
struct Images {
    busybox: PathBuf,
    calls: AtomicUsize,
}
#[async_trait]
impl VmImages for Images {
    async fn materialize(&self, image: &CasReference, destination: &VmImageDestination, max_bytes: u64) -> Result<()> {
        let dest = destination.path();
        self.calls.fetch_add(1, Ordering::SeqCst);
        ensure!(*image == image_reference("ab"), "fixture image is not authorized");
        ensure!(self.busybox.metadata()?.len() <= max_bytes, "image exceeds rootfs budget");
        for directory in ["bin", "bin/雪", "dev", "proc", "sys", "tmp", "work", "work/雪"] {
            std::fs::create_dir_all(dest.join(directory))?;
        }
        std::fs::copy(&self.busybox, dest.join("bin/busybox"))?;
        std::os::unix::fs::symlink("busybox", dest.join("bin/sh"))?;
        std::os::unix::fs::symlink("../busybox", dest.join("bin/雪/sh"))?;
        Ok(())
    }
}
fn native_config(root: PathBuf) -> Result<VmConfig> {
    Ok(VmConfig {
        runner: required_path("LOOM_VM_RUNNER"),
        library: required_path("LOOM_LIBKRUN"),
        bwrap: required_path("LOOM_BWRAP"),
        root,
        runtime_roots: std::env::split_paths(
            &std::env::var_os("LOOM_VM_RUNTIME_ROOTS").context("native VM tests require LOOM_VM_RUNTIME_ROOTS")?,
        )
        .collect(),
    })
}
struct Fixture {
    directory: tempfile::TempDir,
    supervisor: Supervisor,
    registry: Arc<loom_actor::CompositeRegistry>,
    images: Arc<Images>,
}
impl Fixture {
    fn new() -> Result<Self> {
        ensure!(Path::new("/dev/kvm").exists(), "native VM tests require /dev/kvm");
        let directory = tempfile::tempdir()?;
        let supervisor = Supervisor::new(loom_store::Store::open(directory.path().join("process.sqlite"))?)?;
        let root = directory.path().join("vms");
        std::fs::create_dir(&root)?;
        let images = Arc::new(Images { busybox: required_path("LOOM_STATIC_BUSYBOX"), calls: AtomicUsize::new(0) });
        let config = native_config(root)?;
        let driver = Arc::new(VmDriver::new(supervisor.clone(), config, images.clone())?);
        let mut registry = registry::Registry::new();
        registry.insert(HASH.into(), Arc::new(VmActor));
        let registry = Arc::new(loom_actor::CompositeRegistry::new(Arc::new(registry), vec![], vec![driver])?);
        Ok(Self { directory, supervisor, registry, images })
    }
    async fn node(&self) -> Node {
        Node::new(self.directory.path().join("actors"), self.registry.clone(), Arc::new(DefaultEffects), Config::default()).await.unwrap()
    }
    fn spec(&self, script: &str) -> VmSpec {
        VmSpec {
            image: image_reference("ab"),
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: BTreeMap::new(),
            cwd: "/work".into(),
            memory_mb: 256,
            cpus: 1,
            rootfs_mb: 32,
            ttl_ms: 60_000,
            network: VmNetwork::None,
        }
    }
}
async fn spawn(node: &Node, spec: &VmSpec) -> Actor {
    let id = node.spawn_root(HASH, &serde_json::to_vec(&serde_json::json!({"vm":spec})).unwrap()).await.unwrap();
    node.run_until_idle().await.unwrap();
    node.open(&id).await.unwrap()
}
async fn phase(actor: &Actor) -> String {
    actor.sql("SELECT phase FROM process_state", ()).await.unwrap().rows[0].get(0).unwrap()
}
async fn wait_phase(node: &Node, actor: &Actor, expected: &str) {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            node.run_until_idle().await.unwrap();
            let current = phase(actor).await;
            if current == expected {
                return;
            }
            assert!(!terminal(&current), "VM reached {current}, expected {expected}: {}", diagnostics(actor).await);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("VM did not reach {expected}"));
}
fn terminal(phase: &str) -> bool {
    matches!(phase, "completed" | "cancelled" | "failed" | "interrupted")
}
async fn diagnostics(actor: &Actor) -> String {
    let row = actor.sql("SELECT phase, coalesce(code,-999), coalesce(error,'') FROM process_state", ()).await.unwrap();
    format!(
        "phase={} code={} error={} stdout={:?} stderr={:?}",
        row.rows[0].get::<String>(0).unwrap(),
        row.rows[0].get::<i64>(1).unwrap(),
        row.rows[0].get::<String>(2).unwrap(),
        String::from_utf8_lossy(&output(actor, "stdout").await),
        String::from_utf8_lossy(&output(actor, "stderr").await)
    )
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
#[tokio::test]
async fn vm_boots_unicode_exit_and_private_root() -> Result<()> {
    let fixture = Fixture::new()?;
    let node = fixture.node().await;
    let host_secret = fixture.directory.path().join("host-secret");
    std::fs::write(&host_secret, "outside\n")?;
    let mut spec = fixture.spec("if (read x < \"$HOST_SECRET\") 2>/dev/null; then exit 41; fi; printf private > /guest-only; printf '雪😀 café'; printf diagnostic >&2; exit 7");
    spec.env.insert("HOST_SECRET".into(), host_secret.to_str().unwrap().into());
    let actor = spawn(&node, &spec).await;
    wait_phase(&node, &actor, "completed").await;
    assert_eq!(output(&actor, "stdout").await, "雪😀 café".as_bytes(), "{}", diagnostics(&actor).await);
    assert_eq!(output(&actor, "stderr").await, b"diagnostic", "{}", diagnostics(&actor).await);
    assert_eq!(actor.sql("SELECT code FROM process_state", ()).await?.rows[0].get::<i64>(0)?, 7, "{}", diagnostics(&actor).await);
    assert!(!fixture.directory.path().join("guest-only").exists());
    assert_eq!(std::fs::read_to_string(host_secret)?, "outside\n");
    node.close().await?;
    Ok(())
}
#[tokio::test]
async fn vm_guest_observes_memory_and_cpu_limits() -> Result<()> {
    let fixture = Fixture::new()?;
    let node = fixture.node().await;
    let actor = spawn(
        &node,
        &fixture.spec("/bin/busybox awk '/MemTotal:/ {print $2}' /proc/meminfo; /bin/busybox grep -c '^processor' /proc/cpuinfo"),
    )
    .await;
    wait_phase(&node, &actor, "completed").await;
    let text = String::from_utf8(output(&actor, "stdout").await)?;
    let evidence = diagnostics(&actor).await;
    let readings: Vec<u64> = text.lines().map(str::parse).collect::<std::result::Result<_, _>>().with_context(|| evidence.clone())?;
    assert_eq!(readings.len(), 2, "{evidence}");
    assert!((64 * 1024..=256 * 1024).contains(&readings[0]), "guest memory: {evidence}");
    assert_eq!(readings[1], 1, "guest CPUs: {evidence}");
    node.close().await?;
    Ok(())
}
#[tokio::test]
async fn vm_owner_stop_reopen_does_not_boot() -> Result<()> {
    let fixture = Fixture::new()?;
    let node = fixture.node().await;
    let actor = spawn(&node, &fixture.spec("printf READY; /bin/busybox sleep 300")).await;
    wait_phase(&node, &actor, "running").await;
    // Running acknowledges the host launcher; guest output proves KVM booted.
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            node.run_until_idle().await.unwrap();
            if output(&actor, "stdout").await == b"READY" {
                break;
            }
            assert!(!terminal(&phase(&actor).await), "VM exited before guest READY: {}", diagnostics(&actor).await);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("guest did not boot before owner-stop test")?;
    let state = fixture.supervisor.list()?;
    assert_eq!(state.len(), 1);
    node.stop(actor.id(), "test owner stop").await?;
    let terminal = tokio::time::timeout(Duration::from_secs(15), fixture.supervisor.wait(&state[0].id)).await??;
    assert_eq!(terminal.phase, Phase::Cancelled);
    node.close().await?;
    drop(actor);
    drop(node);
    let reopened = fixture.node().await;
    reopened.run_until_idle().await?;
    assert_eq!(fixture.supervisor.list()?.len(), 1);
    assert_eq!(fixture.images.calls.load(Ordering::SeqCst), 1);
    reopened.close().await?;
    Ok(())
}
#[tokio::test]
async fn vm_ttl_cancels_guest() -> Result<()> {
    let fixture = Fixture::new()?;
    let node = fixture.node().await;
    let mut spec = fixture.spec("/bin/busybox sleep 300");
    spec.ttl_ms = 3000;
    let actor = spawn(&node, &spec).await;
    wait_phase(&node, &actor, "failed").await;
    let error: String = actor.sql("SELECT error FROM process_state", ()).await?.rows[0].get(0)?;
    assert!(error.contains("VM TTL expired"), "wrong VM failure: {error}");
    let state = fixture.supervisor.list()?;
    assert_eq!(state.len(), 1);
    assert_eq!(fixture.supervisor.wait(&state[0].id).await?.phase, Phase::Cancelled);
    node.close().await?;
    Ok(())
}
#[tokio::test]
async fn vm_image_refusal_starts_no_process() -> Result<()> {
    let fixture = Fixture::new()?;
    let node = fixture.node().await;
    let mut spec = fixture.spec("exit 0");
    spec.image = image_reference("cd");
    let actor = spawn(&node, &spec).await;
    wait_phase(&node, &actor, "failed").await;
    assert!(fixture.supervisor.list()?.is_empty());
    assert_eq!(fixture.images.calls.load(Ordering::SeqCst), 1);
    node.close().await?;
    Ok(())
}

#[tokio::test]
async fn vm_guest_rootfs_budget_rejects_oversized_write() -> Result<()> {
    let fixture = Fixture::new()?;
    let node = fixture.node().await;
    // Keep diagnostics on the console: redirecting them to the full guest disk
    // would destroy the evidence distinguishing ENOSPC from other dd failures.
    let actor = spawn(
        &node,
        &fixture.spec("if /bin/busybox dd if=/dev/zero of=/work/fill bs=1048576 count=40; then exit 42; fi; printf budget-enforced"),
    )
    .await;
    wait_phase(&node, &actor, "completed").await;
    assert_eq!(actor.sql("SELECT code FROM process_state", ()).await?.rows[0].get::<i64>(0)?, 0, "{}", diagnostics(&actor).await);
    assert_eq!(output(&actor, "stdout").await, b"budget-enforced", "{}", diagnostics(&actor).await);
    let diagnostics = String::from_utf8(output(&actor, "stderr").await)?;
    assert!(diagnostics.contains("No space left on device"), "wrong guest write failure: {diagnostics}");
    node.close().await?;
    Ok(())
}

#[test]
fn vm_root_lock_excludes_second_driver_and_releases_on_drop() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("vms");
    std::fs::create_dir(&root)?;
    let supervisor = Supervisor::new(loom_store::Store::memory()?)?;
    let images = Arc::new(Images { busybox: required_path("LOOM_STATIC_BUSYBOX"), calls: AtomicUsize::new(0) });
    let first = VmDriver::new(supervisor.clone(), native_config(root.clone())?, images.clone())?;
    let active = root.join("vm-active-witness");
    std::fs::create_dir(&active)?;
    std::fs::write(active.join("untouched"), "active")?;
    assert!(
        VmDriver::new(supervisor.clone(), native_config(root.clone())?, images.clone()).is_err(),
        "second driver acquired an owned root"
    );
    assert_eq!(std::fs::read_to_string(active.join("untouched"))?, "active", "rejected constructor deleted active workspace");
    drop(first);
    let reopened = VmDriver::new(supervisor, native_config(root)?, images)?;
    assert!(!active.exists(), "reopened owner did not recover stale workspace");
    drop(reopened);
    Ok(())
}

#[test]
fn vm_recovers_restricted_stale_workspace_without_following_symlinks() -> Result<()> {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("vms");
    std::fs::create_dir(&root)?;
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside)?;
    std::fs::write(outside.join("secret"), "preserved")?;
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o500))?;
    let stale = root.join("vm-abandoned");
    let restricted = stale.join("image/locked");
    std::fs::create_dir_all(&restricted)?;
    std::fs::write(restricted.join("data"), "stale")?;
    symlink(&outside, stale.join("external-link"))?;
    symlink(&outside, root.join("vm-symlink"))?;
    std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0))?;
    std::fs::set_permissions(stale.join("image"), std::fs::Permissions::from_mode(0))?;
    std::fs::write(root.join("unrelated"), "keep")?;
    let images = Arc::new(Images { busybox: required_path("LOOM_STATIC_BUSYBOX"), calls: AtomicUsize::new(0) });
    let supervisor = Supervisor::new(loom_store::Store::memory()?)?;
    let driver = VmDriver::new(supervisor, native_config(root.clone())?, images)?;
    assert!(!stale.exists(), "restricted stale workspace survived recovery");
    assert_eq!(std::fs::read_to_string(outside.join("secret"))?, "preserved");
    assert_eq!(outside.metadata()?.permissions().mode() & 0o777, 0o500, "recovery changed a symlink target's permissions");
    assert_eq!(std::fs::read_to_string(root.join("unrelated"))?, "keep");
    drop(driver);
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[tokio::test]
async fn vm_preserves_exact_guest_launch_values() -> Result<()> {
    let fixture = Fixture::new()?;
    let node = fixture.node().await;
    let mut spec = fixture.spec(
        r#"printf 'cwd=%s\n' "$PWD"; printf 'args='; printf '[%s]' "$@"; printf '\nenv=%s\nKRUN_INIT=%s\nKRUN_WORKDIR=%s\nKRUN_INIT_PID1=%s\n' "$EXACT_ENV" "$KRUN_INIT" "$KRUN_WORKDIR" "$KRUN_INIT_PID1""#,
    );
    spec.command = "/bin/雪/sh".into();
    spec.cwd = "/work/雪".into();
    // The object-key-looking values catch parsers that mistake nested Cmd
    // strings for configuration keys. Control bytes must survive JSON escapes.
    spec.args.extend([
        "transport-check".into(),
        String::new(),
        "two words".into(),
        "\"quoted\"\\path".into(),
        "line1\nline2".into(),
        "\u{1}".into(),
        "WorkingDir".into(),
        "Env".into(),
    ]);
    spec.env.insert("EXACT_ENV".into(), "雪😀 \"quotes\" \\ slash\nsecond\u{1}".into());
    // These are application environment values. They cannot override the
    // typed command/cwd or opt out of the VM's normal init/exit supervision.
    spec.env.insert("KRUN_INIT".into(), "/wrong-command".into());
    spec.env.insert("KRUN_WORKDIR".into(), "/wrong-directory".into());
    spec.env.insert("KRUN_INIT_PID1".into(), "1".into());
    let actor = spawn(&node, &spec).await;
    wait_phase(&node, &actor, "completed").await;
    let diagnostic = diagnostics(&actor).await;
    let expected = "cwd=/work/雪\nargs=[][two words][\"quoted\"\\path][line1\nline2][\u{1}][WorkingDir][Env]\nenv=雪😀 \"quotes\" \\ slash\nsecond\u{1}\nKRUN_INIT=/wrong-command\nKRUN_WORKDIR=/wrong-directory\nKRUN_INIT_PID1=1\n";
    assert_eq!(output(&actor, "stdout").await, expected.as_bytes(), "{diagnostic}");
    assert!(output(&actor, "stderr").await.is_empty(), "{diagnostic}");
    assert_eq!(actor.sql("SELECT code FROM process_state", ()).await?.rows[0].get::<i64>(0)?, 0, "{diagnostic}");
    node.close().await?;
    Ok(())
}
