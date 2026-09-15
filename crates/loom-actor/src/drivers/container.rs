//! Temporary Docker resources use the same durable process mailbox and I/O pump.
use super::{Driver, DriverContext, DriverDelivery, process::run_session};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use loom_process::{ProcessSpec, Supervisor};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tokio::sync::mpsc;

pub const HASH: &str = "container-runtime-v1";
#[derive(Clone)]
pub struct DockerConfig {
    pub executable: PathBuf,
    pub host: Option<String>,
    pub root: PathBuf,
    pub tenant: String,
    pub node: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerSpec {
    pub image: String,
    #[serde(default)]
    pub network: Network,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_memory")]
    pub memory_mb: u64,
    #[serde(default = "default_cpus")]
    pub cpus: f64,
    #[serde(default = "default_pids")]
    pub pids: u32,
    #[serde(default = "default_ttl")]
    pub ttl_ms: u64,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    None,
    #[default]
    Bridge,
}
fn default_memory() -> u64 {
    512
}
fn default_cpus() -> f64 {
    1.0
}
fn default_pids() -> u32 {
    128
}
fn default_ttl() -> u64 {
    3_600_000
}
impl ContainerSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.image.is_empty()
                && self.image.len() <= 512
                && !self.image.starts_with('-')
                && !self.image.chars().any(char::is_whitespace),
            "invalid container image"
        );
        ensure!((16..=32768).contains(&self.memory_mb), "memoryMb must be 16..32768");
        ensure!(self.cpus.is_finite() && (0.1..=8.0).contains(&self.cpus), "cpus must be 0.1..8");
        ensure!((1..=4096).contains(&self.pids), "pids must be 1..4096");
        ensure!((1..=86_400_000).contains(&self.ttl_ms), "ttlMs must be 1..86400000");
        ensure!(self.args.len() <= 256 && self.env.len() <= 128, "container args/env too large");
        ensure!(self.env.keys().all(|key| !key.is_empty() && !key.contains(['=', '\0'])), "invalid container environment key");
        ensure!(self.command.as_ref().is_none_or(|v| !v.is_empty() && !v.contains('\0')), "invalid container command");
        ensure!(
            self.args.iter().chain(self.env.values()).all(|v| v.len() <= 65536 && !v.contains('\0')),
            "invalid container argument/environment value"
        );
        Ok(())
    }
}
pub struct ContainerDriver {
    supervisor: Supervisor,
    config: DockerConfig,
}
impl ContainerDriver {
    pub fn new(supervisor: Supervisor, config: DockerConfig) -> Result<Self> {
        ensure!(config.executable.is_absolute() && config.root.is_absolute(), "Docker executable and root must be absolute");
        ensure!(
            !config.tenant.is_empty() && config.tenant.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "invalid Docker tenant"
        );
        ensure!(!config.node.is_empty(), "Docker node identity is required");
        Ok(Self { supervisor, config })
    }
    /// Startup runs before admitting actor turns. Native resources from the old
    /// host incarnation are interrupted, never silently re-executed on replay.
    pub async fn recover(&self) -> Result<()> {
        let ids = self
            .config
            .output(&[
                "ps".into(),
                "-aq".into(),
                "--filter".into(),
                format!("label=loom.tenant={}", self.config.tenant),
                "--filter".into(),
                "label=loom.resource=container-v1".into(),
                "--filter".into(),
                format!("label=loom.node={}", self.config.node_key()),
            ])
            .await?;
        for id in ids.split_whitespace() {
            self.config.output(&["rm".into(), "-f".into(), id.into()]).await?;
        }
        Ok(())
    }
}
impl DockerConfig {
    fn node_key(&self) -> String {
        blake3::hash(self.node.as_bytes()).to_hex().to_string()
    }
    fn command(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.executable);
        cmd.env_clear().current_dir(&self.root).kill_on_drop(true);
        if let Some(host) = &self.host {
            cmd.args(["--host", host]);
        }
        cmd
    }
    async fn output(&self, args: &[String]) -> Result<String> {
        let output = tokio::time::timeout(Duration::from_secs(120), self.command().args(args).output())
            .await
            .context("Docker command timed out")??;
        ensure!(output.status.success(), "Docker command failed: {}", String::from_utf8_lossy(&output.stderr));
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }
}
/// Cleanup also runs when a driver future is aborted. A bounded synchronous
/// wait is necessary here: killing `docker start` alone leaves its container
/// running in the daemon. No detached Tokio task may outlive Node shutdown.
struct ContainerGuard {
    config: DockerConfig,
    name: String,
    armed: bool,
}
impl ContainerGuard {
    async fn cleanup(&mut self) -> Result<()> {
        // --rm may already have removed a normally exited resource. Query by
        // its exact name so absence is successful without hiding daemon errors.
        let id = self.config.output(&["ps".into(), "-aq".into(), "--filter".into(), format!("name=^/{}$", self.name)]).await?;
        if !id.is_empty() {
            self.config.output(&["rm".into(), "-f".into(), id]).await?;
        }
        self.armed = false;
        Ok(())
    }
}
impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut command = std::process::Command::new(&self.config.executable);
        command.env_clear().current_dir(&self.config.root);
        if let Some(host) = &self.config.host {
            command.args(["--host", host]);
        }
        command.args(["rm", "-f", &self.name]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::inherit());
        match command.spawn() {
            Ok(mut child) => {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            if !status.success() {
                                eprintln!("Docker cleanup failed for {}: {status}", self.name);
                            }
                            break;
                        }
                        Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                        _ => {
                            let _ = child.kill();
                            let _ = child.wait();
                            eprintln!("Docker cleanup timed out for {}; startup recovery will retry", self.name);
                            break;
                        }
                    }
                }
            }
            Err(error) => eprintln!("Docker cleanup failed for {}: {error}", self.name),
        }
    }
}
#[async_trait]
impl Driver for ContainerDriver {
    fn hash(&self) -> &str {
        HASH
    }
    async fn run(&self, cx: DriverContext, init: &[u8], deliveries: mpsc::Receiver<DriverDelivery>) -> Result<()> {
        let spec: ContainerSpec = serde_json::from_slice(init)?;
        spec.validate()?;
        // Driver identities contain owner/local separators (`/`), which Docker
        // rejects in resource names. Hash the complete identity so the external
        // name remains valid without collisions from lossy character replacement.
        let name = format!("loom-{}-{}", &self.config.node_key()[..16], blake3::hash(cx.id().as_bytes()).to_hex());
        let mut guard = ContainerGuard { config: self.config.clone(), name: name.clone(), armed: true };
        let mut args = vec![
            "create".into(),
            "--rm".into(),
            "-i".into(),
            "--name".into(),
            name.clone(),
            "--label".into(),
            "loom.resource=container-v1".into(),
            "--label".into(),
            format!("loom.tenant={}", self.config.tenant),
            "--label".into(),
            format!("loom.owner={}", cx.owner().target),
            "--memory".into(),
            format!("{}m", spec.memory_mb),
            "--memory-swap".into(),
            format!("{}m", spec.memory_mb),
            "--cpus".into(),
            spec.cpus.to_string(),
            "--pids-limit".into(),
            spec.pids.to_string(),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
        ];
        args.extend([
            "--label".into(),
            format!("loom.node={}", self.config.node_key()),
            "--network".into(),
            match spec.network {
                Network::None => "none".into(),
                Network::Bridge => "bridge".into(),
            },
        ]);
        for (key, value) in &spec.env {
            args.extend(["--env".into(), format!("{key}={value}")]);
        }
        if let Some(command) = spec.command {
            args.extend(["--entrypoint".into(), command]);
        }
        args.push(spec.image.clone());
        args.extend(spec.args);
        let container_id = self.config.output(&args).await?;
        let image_id = self.config.output(&["inspect".into(), "--format".into(), "{{.Image}}".into(), container_id.clone()]).await?;
        let mut args = Vec::new();
        if let Some(host) = &self.config.host {
            args.extend(["--host".into(), host.clone()]);
        }
        args.extend(["start".into(), "-ai".into(), container_id.clone()]);
        let session = self
            .supervisor
            .start_session(ProcessSpec {
                machine: format!("container:{}", self.config.tenant),
                program: self.config.executable.to_string_lossy().into_owned(),
                args,
                cwd: self.config.root.clone(),
                root: self.config.root.clone(),
                env: BTreeMap::new(),
                capture_paths: Vec::new(),
            })
            .await?;
        let result = tokio::time::timeout(
            Duration::from_millis(spec.ttl_ms),
            run_session(cx, session, deliveries, serde_json::json!({"container_id":container_id,"image_id":image_id})),
        )
        .await;
        guard.cleanup().await?;
        result.context("container TTL expired")?
    }
}
