//! Filesystem and namespace confinement for interactive processes.
use crate::ProcessSpec;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Host paths are administrator-granted runtime capabilities, including the
/// executable's loader and libraries. Darwin additionally grants its system
/// loader directories and basic null/random devices.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessSandbox {
    #[serde(default)]
    pub readonly: Vec<PathBuf>,
    #[serde(default)]
    pub network: bool,
}

struct Validated {
    root: PathBuf,
    cwd: PathBuf,
    program: PathBuf,
    readonly: Vec<PathBuf>,
}

impl ProcessSandbox {
    pub fn validate(&self, spec: &ProcessSpec) -> Result<()> {
        self.validated(spec)?;
        #[cfg(target_os = "linux")]
        bubblewrap()?;
        #[cfg(target_os = "macos")]
        ensure!(
            Path::new("/usr/bin/sandbox-exec").is_file(),
            "Darwin process sandbox requires /usr/bin/sandbox-exec"
        );
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        anyhow::bail!("no process confinement backend for this platform");
        Ok(())
    }

    fn validated(&self, spec: &ProcessSpec) -> Result<Validated> {
        let root = canonical_absolute(&spec.root).context("sandbox writable root")?;
        reject_broad_path(&root)?;
        ensure!(root.is_dir(), "sandbox root must be a directory");
        let cwd = canonical_absolute(&spec.cwd).context("sandbox cwd")?;
        ensure!(
            cwd.is_dir() && cwd.starts_with(&root),
            "sandbox cwd escapes writable root"
        );
        let program = canonical_absolute(Path::new(&spec.program)).context("sandbox program")?;
        ensure!(program.is_file(), "sandbox program must be a regular file");
        let mut readonly = Vec::new();
        for path in &self.readonly {
            let path = canonical_absolute(path).context("sandbox readonly path")?;
            reject_broad_path(&path)?;
            ensure!(
                path.is_file() || path.is_dir(),
                "sandbox readonly path must be a file or directory"
            );
            ensure!(
                !root.starts_with(&path) && !path.starts_with(&root),
                "sandbox runtime and writable root must not overlap"
            );
            ensure!(
                !["/proc", "/dev", "/tmp"]
                    .iter()
                    .any(|reserved| path.starts_with(reserved)),
                "sandbox readonly path overlaps private proc, dev or tmp"
            );
            readonly.push(path);
        }
        ensure!(
            readonly.iter().any(|path| program.starts_with(path)),
            "sandbox program must be included in readonly runtime paths"
        );
        ensure!(
            !root.starts_with("/proc") && !root.starts_with("/dev"),
            "sandbox writable root overlaps private proc or dev"
        );
        Ok(Validated {
            root,
            cwd,
            program,
            readonly,
        })
    }

    /// Wrap the command before giving it to the existing lifecycle owner.
    /// Host path grants must remain administrator-controlled during launch:
    /// canonicalization is admission validation, not a defense against a host
    /// administrator concurrently replacing the runtime or machine root.
    pub fn wrapped_spec(&self, spec: &ProcessSpec) -> Result<ProcessSpec> {
        let validated = self.validated(spec)?;
        #[cfg(target_os = "linux")]
        {
            let wrapper = bubblewrap()?;
            let mut args: Vec<String> = [
                "--die-with-parent",
                "--new-session",
                "--unshare-all",
                "--cap-drop",
                "ALL",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();
            if self.network {
                args.push("--share-net".into());
            }
            args.extend(
                ["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]
                    .into_iter()
                    .map(str::to_owned),
            );
            args.push("--clearenv".into());
            for (name, value) in &spec.env {
                args.extend(["--setenv".into(), name.clone(), value.clone()]);
            }
            for path in validated.readonly {
                args.extend(["--ro-bind".into(), utf8(&path)?, utf8(&path)?]);
            }
            args.extend([
                "--bind".into(),
                utf8(&validated.root)?,
                utf8(&validated.root)?,
                "--chdir".into(),
                utf8(&validated.cwd)?,
                "--".into(),
                utf8(&validated.program)?,
            ]);
            args.extend(spec.args.iter().cloned());
            let mut wrapped = spec.clone();
            wrapped.program = utf8(&wrapper)?;
            wrapped.args = args;
            // Guest LD_PRELOAD and similar variables must never reach the host wrapper.
            wrapped.env.clear();
            wrapped.root = validated.root;
            wrapped.cwd = validated.cwd;
            Ok(wrapped)
        }
        #[cfg(target_os = "macos")]
        {
            let wrapper = Path::new("/usr/bin/sandbox-exec");
            ensure!(
                wrapper.is_file(),
                "Darwin process sandbox requires /usr/bin/sandbox-exec"
            );
            // Dynamic paths are parameters, never SBPL source. The root literal
            // is required by macOS dyld CacheFinder; it grants no descendants.
            let mut profile = String::from(
                r#"(version 1)
(deny default)
(allow process-exec process-fork)
(allow file-read* (literal "/") (subpath "/System/Library") (subpath "/usr/lib")
  (literal "/private/var/select/sh")
  (literal "/usr/bin/env") (literal "/dev/null") (literal "/dev/random") (literal "/dev/urandom"))
(allow file-write* (literal "/dev/null"))
(allow file-read* file-write* (subpath (param "ROOT")))
"#,
            );
            if self.network {
                profile.push_str("(allow network*)\n");
            }
            let mut parameters = vec!["-D".to_owned(), format!("ROOT={}", utf8(&validated.root)?)];
            for (index, path) in validated.readonly.iter().enumerate() {
                let name = format!("RUNTIME_{index}");
                let filter = if path.is_dir() { "subpath" } else { "literal" };
                profile.push_str(&format!(
                    "(allow file-read* ({filter} (param \"{name}\")))\n"
                ));
                parameters.extend(["-D".into(), format!("{name}={}", utf8(path)?)]);
            }
            let mut args = vec!["-p".to_owned(), profile];
            args.extend(parameters);
            // Apply guest environment only after confinement: loader injection
            // variables must not affect the unsandboxed sandbox-exec wrapper.
            args.extend(["/usr/bin/env".into(), "-i".into()]);
            for (name, value) in &spec.env {
                ensure!(
                    !name.is_empty() && !name.contains('=') && !name.starts_with('-'),
                    "invalid sandbox environment name"
                );
                args.push(format!("{name}={value}"));
            }
            args.push(utf8(&validated.program)?);
            args.extend(spec.args.iter().cloned());
            let mut wrapped = spec.clone();
            wrapped.program = utf8(wrapper)?;
            wrapped.args = args;
            wrapped.env.clear();
            wrapped.root = validated.root;
            wrapped.cwd = validated.cwd;
            Ok(wrapped)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = validated;
            anyhow::bail!(
                "process sandbox requires Linux bubblewrap or Darwin sandbox-exec; no backend for this platform"
            )
        }
    }
}

fn canonical_absolute(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "sandbox paths must be absolute: {}",
        path.display()
    );
    path.canonicalize()
        .with_context(|| format!("resolve {}", path.display()))
}

fn reject_broad_path(path: &Path) -> Result<()> {
    ensure!(
        ![
            "/",
            "/home",
            "/Users",
            "/root",
            "/tmp",
            "/var",
            "/private",
            "/run",
            "/nix",
            "/nix/store"
        ]
        .iter()
        .any(|broad| path == Path::new(broad)),
        "sandbox path grants a broad host root: {}",
        path.display()
    );
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home)
            .canonicalize()
            .context("resolve supervisor home")?;
        ensure!(
            !home.starts_with(path),
            "sandbox path exposes supervisor home: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn utf8(path: &Path) -> Result<String> {
    Ok(path
        .to_str()
        .context("sandbox path is not UTF-8")?
        .to_owned())
}

#[cfg(target_os = "linux")]
fn checked_wrapper(path: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    ensure!(
        path.is_absolute(),
        "bubblewrap path must be absolute: {}",
        path.display()
    );
    let metadata = path
        .metadata()
        .with_context(|| format!("bubblewrap executable {}", path.display()))?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        "bubblewrap path is not an executable file: {}",
        path.display()
    );
    // Preserve invocation-name behavior for explicitly supplied wrappers.
    Ok(path.to_owned())
}

#[cfg(target_os = "linux")]
fn bubblewrap() -> Result<PathBuf> {
    // The packaged crate carries its actual runtime dependency. A host override
    // remains explicit; a broken configured path must not select another binary.
    if let Some(path) = std::env::var_os("LOOM_BWRAP")
        .or_else(|| option_env!("LOOM_BWRAP").map(std::ffi::OsString::from))
    {
        return checked_wrapper(Path::new(&path)).context("configured LOOM_BWRAP");
    }
    // Source-tree builds can use the supervisor's PATH. Guest process env is
    // applied only inside confinement and cannot select the host wrapper.
    let search =
        std::env::var_os("PATH").context("supervisor PATH missing; configure LOOM_BWRAP")?;
    for directory in std::env::split_paths(&search).filter(|directory| directory.is_absolute()) {
        let candidate = directory.join("bwrap");
        if candidate.exists() {
            return checked_wrapper(&candidate);
        }
    }
    anyhow::bail!(
        "Linux process sandbox requires bubblewrap: configure LOOM_BWRAP or install bwrap on the supervisor's absolute PATH"
    )
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::{Phase, ProcessEvent, Supervisor};
    use loom_store::Store;
    use std::{collections::BTreeMap, time::Duration};

    #[tokio::test]
    async fn sandbox_denies_outside_read_and_write() -> Result<()> {
        #[cfg(target_os = "linux")]
        let runtime = PathBuf::from(std::env::var_os("LOOM_STATIC_BUSYBOX").context(
            "set LOOM_STATIC_BUSYBOX to a static busybox binary for Linux confinement tests",
        )?)
        .canonicalize()?;
        #[cfg(target_os = "macos")]
        let runtime = PathBuf::from("/bin/sh").canonicalize()?;
        let mut args = Vec::new();
        #[cfg(target_os = "linux")]
        args.push("sh".into());
        // Linux may allow these writes into its private /tmp namespace, while
        // Darwin denies them. The host-side assertions below prove isolation;
        // requiring a write error would confuse private scratch with host access.
        args.extend(["-c".into(), "if (read x < \"$OUTSIDE\") 2>/dev/null; then exit 41; fi; (printf overwrite > \"$OUTSIDE\") 2>/dev/null || :; (printf created > \"$OUTSIDE_NEW\") 2>/dev/null || :; printf inside > inside; printf confined".into()]);
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("machine");
        std::fs::create_dir(&root)?;
        let secret = temp.path().join("secret");
        let outside_new = temp.path().join("guest-created");
        std::fs::write(&secret, "host-secret\n")?;
        let spec = ProcessSpec {
            machine: "confined".into(),
            program: utf8(&runtime)?,
            args,
            root: root.clone(),
            cwd: root.clone(),
            env: BTreeMap::from([
                ("OUTSIDE".into(), utf8(&secret)?),
                ("OUTSIDE_NEW".into(), utf8(&outside_new)?),
            ]),
            capture_paths: Vec::new(),
        };
        let policy = ProcessSandbox {
            readonly: vec![runtime],
            network: false,
        };
        let supervisor = Supervisor::new(Store::memory()?)?;
        let mut session = supervisor.start_sandboxed_session(spec, &policy).await?;
        session.close_stdin().await?;
        let mut output = Vec::new();
        let state = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = session.next_event().await {
                match event {
                    ProcessEvent::Output {
                        bytes,
                        stderr: false,
                        ..
                    } => output.extend(bytes),
                    ProcessEvent::Exit { state } => return Ok(state),
                    _ => {}
                }
            }
            anyhow::bail!("sandbox event stream ended without exit")
        })
        .await??;
        ensure!(
            state.phase == Phase::Completed && state.code == Some(0),
            "sandbox failed: {state:?}"
        );
        ensure!(output == b"confined", "unexpected sandbox output");
        ensure!(
            std::fs::read_to_string(secret)? == "host-secret\n",
            "host secret modified"
        );
        ensure!(
            !outside_new.exists(),
            "guest created a file outside the host writable root"
        );
        ensure!(
            std::fs::read_to_string(root.join("inside"))? == "inside",
            "writable root unavailable"
        );
        Ok(())
    }
}
