//! One temporary Linux VM per committed driver, using the shared process owner.
use super::{Driver, DriverContext, DriverDelivery, process::run_session};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use loom_process::{ProcessSpec, Supervisor};
use loom_proto::{CasReference, VmLaunch, VmSpec};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

pub const HASH: &str = "linux-vm-runtime-v1";

/// Materialization is tenant-authorized CAS access, never a guest-supplied host
/// path. Implementations validate the complete manifest before publishing files
/// and enforce max_bytes. Blocking work retains a cloned destination until it
/// exits, so cancellation cannot remove its workspace while writes are in flight.
#[async_trait]
pub trait VmImages: Send + Sync {
    async fn materialize(&self, image: &CasReference, dest: &VmImageDestination, max_bytes: u64) -> Result<()>;
}

/// A materializer's filesystem lifetime travels with its work, including work
/// on a blocking thread. The last owner removes the private image directory.
#[derive(Clone)]
pub struct VmImageDestination {
    workspace: Arc<Workspace>,
    path: PathBuf,
}
struct Workspace {
    directory: Arc<tempfile::TempDir>,
    // A cancelled materializer may still be finishing one blocking syscall.
    // Keep admission locked until its last destination clone is dropped.
    _lease: Option<Arc<File>>,
}
impl Drop for Workspace {
    fn drop(&mut self) {
        // Image modes belong to the guest and can remove owner write/search.
        // Restore only our private host directory permissions for final removal.
        if let Err(error) = make_removable(self.directory.path()) {
            eprintln!("VM workspace cleanup failed: {error:#}");
        }
    }
}
impl VmImageDestination {
    pub fn new(workspace: Arc<tempfile::TempDir>) -> Self {
        Self::with_lease(workspace, None)
    }
    fn with_lease(directory: Arc<tempfile::TempDir>, lease: Option<Arc<File>>) -> Self {
        Self { path: directory.path().join("image"), workspace: Arc::new(Workspace { directory, _lease: lease }) }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    fn workspace(&self) -> &Path {
        self.workspace.directory.path()
    }
}

/// All host paths are fixed at daemon admission. Only the materialized image
/// and an exact runtime closure enter the launcher namespace.
#[derive(Clone, Debug)]
pub struct VmConfig {
    pub runner: PathBuf,
    pub library: PathBuf,
    pub bwrap: PathBuf,
    pub root: PathBuf,
    pub runtime_roots: Vec<PathBuf>,
}
impl VmConfig {
    fn validated(mut self) -> Result<Self> {
        ensure!(cfg!(all(target_os = "linux", target_arch = "x86_64")), "Linux VM runtime requires x86_64 Linux with KVM");
        self.runner = canonical(&self.runner)?;
        self.library = canonical(&self.library)?;
        self.bwrap = canonical(&self.bwrap)?;
        self.root = canonical(&self.root)?;
        ensure!(self.runner.is_file() && self.library.is_file() && self.bwrap.is_file(), "VM runner, library and bubblewrap must be files");
        ensure!(self.root.is_dir(), "VM work root must be a directory");
        let mut roots = Vec::new();
        for path in self.runtime_roots {
            let path = canonical(&path)?;
            ensure!(path.is_file() || path.is_dir(), "VM runtime path is not a file or directory");
            ensure!(
                !["/", "/home", "/Users", "/root", "/tmp", "/var", "/run", "/nix", "/nix/store"]
                    .iter()
                    .any(|broad| path == Path::new(broad)),
                "VM runtime closure contains a broad host path"
            );
            ensure!(
                !["/proc", "/dev", "/base", "/guest", "/config.json"].iter().any(|reserved| path.starts_with(reserved)),
                "VM runtime path overlaps a launcher mount"
            );
            ensure!(!self.root.starts_with(&path) && !path.starts_with(&self.root), "VM runtime closure overlaps writable work root");
            roots.push(path);
        }
        ensure!(roots.iter().any(|path| self.runner.starts_with(path)), "VM runner missing from runtime closure");
        ensure!(roots.iter().any(|path| self.library.starts_with(path)), "libkrun missing from runtime closure");
        let mut seen = BTreeSet::new();
        roots.retain(|root| seen.insert(root.clone()));
        self.runtime_roots = roots;
        Ok(self)
    }

    fn loader_path(&self) -> Result<String> {
        let mut directories = Vec::new();
        let mut seen = BTreeSet::new();
        for root in &self.runtime_roots {
            for name in ["lib", "lib64"] {
                let path = root.join(name);
                if path.is_dir() && seen.insert(path.clone()) {
                    directories.push(path);
                }
            }
        }
        std::env::join_paths(directories)?.into_string().map_err(|_| anyhow::anyhow!("VM loader paths must be UTF-8"))
    }
}
/// Never follow image symlinks during cleanup: their targets use guest paths
/// and must not let teardown chmod or delete anything outside this workspace.
fn make_removable(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_dir() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode() | 0o700;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        }
        for entry in std::fs::read_dir(path)? {
            pending.push(entry?.path());
        }
    }
    Ok(())
}
fn claim_root(root: &Path) -> Result<Arc<File>> {
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(root.join(".owner.lock"))?;
    lock.try_lock().with_context(|| format!("VM workspace root already in use: {}", root.display()))?;
    // The exclusive root owner proves these cannot belong to a live VM or a
    // materializer from the preceding daemon incarnation. VM execution itself
    // died with its parent; replay never launches it again.
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("vm-") {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            make_removable(&entry.path())?;
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(Arc::new(lock))
}

fn canonical(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "VM host paths must be absolute: {}", path.display());
    path.canonicalize().with_context(|| format!("resolve VM host path {}", path.display()))
}
fn utf8(path: &Path) -> Result<String> {
    path.to_str().map(str::to_owned).context("VM host path must be UTF-8")
}

pub struct VmDriver {
    supervisor: Supervisor,
    config: VmConfig,
    images: Arc<dyn VmImages>,
    root_lease: Arc<File>,
}
impl VmDriver {
    pub fn new(supervisor: Supervisor, config: VmConfig, images: Arc<dyn VmImages>) -> Result<Self> {
        let config = config.validated()?;
        let root_lease = claim_root(&config.root)?;
        Ok(Self { supervisor, config, images, root_lease })
    }

    async fn run_vm(&self, cx: DriverContext, spec: VmSpec, deliveries: mpsc::Receiver<DriverDelivery>) -> Result<()> {
        let destination = VmImageDestination::with_lease(
            Arc::new(tempfile::Builder::new().prefix("vm-").tempdir_in(&self.config.root)?),
            Some(self.root_lease.clone()),
        );
        let source = destination.path();
        let rootfs_bytes = spec.rootfs_mb.checked_mul(1024 * 1024).context("VM rootfs size overflow")?;
        self.images.materialize(&spec.image, &destination, rootfs_bytes).await.context("authorize and materialize VM image")?;
        // KVM is the sole host device supplied to the monitor. An inaccessible
        // device is an explicit driver failure; there is no process fallback.
        let _kvm = std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").context("Linux VM requires accessible /dev/kvm")?;
        let launch = VmLaunch { source_root: "/base".into(), guest_root: "/guest".into(), spec: spec.clone() };
        let config = destination.workspace().join("config.json");
        std::fs::write(&config, serde_json::to_vec(&launch)?)?;
        let mut args = vec![
            "--die-with-parent".into(),
            "--unshare-all".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--clearenv".into(),
            "--proc".into(),
            "/proc".into(),
            "--dev".into(),
            "/dev".into(),
            "--dev-bind".into(),
            "/dev/kvm".into(),
            "/dev/kvm".into(),
            "--tmpfs".into(),
            "/tmp".into(),
        ];
        for path in &self.config.runtime_roots {
            args.extend(["--ro-bind".into(), utf8(path)?, utf8(path)?]);
        }
        // Native Cargo runners may lack ELF RUNPATH even when every dependency
        // is mounted. Search only lib/lib64 in the declared host runtime closure;
        // guest environment is passed separately through the libkrun API.
        let loader_path = self.config.loader_path()?;
        if !loader_path.is_empty() {
            args.extend(["--setenv".into(), "LD_LIBRARY_PATH".into(), loader_path]);
        }
        args.extend([
            "--ro-bind".into(),
            utf8(source)?,
            "/base".into(),
            "--ro-bind".into(),
            utf8(&config)?,
            "/config.json".into(),
            "--size".into(),
            rootfs_bytes.to_string(),
            "--tmpfs".into(),
            "/guest".into(),
            "--chdir".into(),
            "/".into(),
            "--".into(),
            utf8(&self.config.runner)?,
            "--config".into(),
            "/config.json".into(),
            "--library".into(),
            utf8(&self.config.library)?,
        ]);
        let session = self
            .supervisor
            .start_session(ProcessSpec {
                machine: format!("vm:{}", cx.id()),
                program: utf8(&self.config.bwrap)?,
                args,
                root: destination.workspace().into(),
                cwd: destination.workspace().into(),
                env: BTreeMap::new(),
                capture_paths: Vec::new(),
            })
            .await?;
        let metadata = serde_json::json!({
            "vm_id":cx.id(), "host_pid":session.host_pid(), "image":spec.image,
            "memoryMb":spec.memory_mb, "cpus":spec.cpus, "rootfsMb":spec.rootfs_mb, "network":spec.network,
        });
        // run_session owns cancellation and its bounded I/O queues. Its drop
        // kills the monitor before workspace removal; the VM root exists only
        // in that process's private, size-bounded tmpfs namespace.
        run_session(cx, session, deliveries, metadata).await
    }
}
#[async_trait]
impl Driver for VmDriver {
    fn hash(&self) -> &str {
        HASH
    }
    async fn run(&self, cx: DriverContext, init: &[u8], deliveries: mpsc::Receiver<DriverDelivery>) -> Result<()> {
        let spec: VmSpec = serde_json::from_slice(init)?;
        spec.validate().map_err(anyhow::Error::msg)?;
        tokio::time::timeout(Duration::from_millis(spec.ttl_ms), self.run_vm(cx, spec, deliveries)).await.context("VM TTL expired")?
    }
}
