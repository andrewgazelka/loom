//! The driver pin owns the compiler selection for every guest operation.
use crate::BuildError;
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub struct GuestToolchain {
    pub channel: String,
    pub rustc: PathBuf,
    pub cargo: PathBuf,
    pub sysroot: PathBuf,
    pub version: String,
}

impl GuestToolchain {
    pub(crate) fn configure(&self, command: &mut Command) -> Result<(), BuildError> {
        let ambient = std::env::var_os("PATH").unwrap_or_default();
        let paths =
            std::iter::once(self.sysroot.join("bin")).chain(std::env::split_paths(&ambient));
        command
            .env("PATH", std::env::join_paths(paths).map_err(rejected)?)
            .env("RUSTUP_TOOLCHAIN", &self.channel)
            .env("RUSTC", &self.rustc);
        Ok(())
    }
}

fn rejected(error: impl std::fmt::Display) -> BuildError {
    BuildError::Rejected(error.to_string())
}

async fn output(command: &mut Command, owner: &str) -> Result<String, BuildError> {
    let result = command
        .output()
        .await
        .map_err(|error| rejected(format!("{owner}: {error}")))?;
    if !result.status.success() {
        return Err(rejected(format!(
            "{owner}: {}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    String::from_utf8(result.stdout).map_err(rejected)
}

/// Resolve the repository's pinned guest compiler, validating any explicit
/// RUSTC override against that compiler before it can be used.
pub async fn resolve_guest_toolchain(root: &Path) -> Result<GuestToolchain, BuildError> {
    resolve(root, std::env::var_os("RUSTC")).await
}

async fn resolve(
    root: &Path,
    override_rustc: Option<std::ffi::OsString>,
) -> Result<GuestToolchain, BuildError> {
    let pin = root.join("tools/hash-rustc/rust-toolchain.toml");
    let text = tokio::fs::read_to_string(&pin)
        .await
        .map_err(|error| rejected(format!("guest compiler pin {}: {error}", pin.display())))?;
    let document: toml::Value = toml::from_str(&text).map_err(rejected)?;
    let channel = document
        .get("toolchain")
        .and_then(|value| value.get("channel"))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| {
            rejected(format!(
                "guest compiler pin {} has no toolchain.channel",
                pin.display()
            ))
        })?
        .to_owned();
    let locate = |binary: &str| {
        let mut command = Command::new("rustup");
        command.args(["which", "--toolchain", &channel, binary]);
        command
    };
    let rustc = PathBuf::from(
        output(
            &mut locate("rustc"),
            &format!("pinned guest rustc {channel}"),
        )
        .await?
        .trim(),
    );
    let cargo = PathBuf::from(
        output(
            &mut locate("cargo"),
            &format!("pinned guest cargo {channel}"),
        )
        .await?
        .trim(),
    );
    let version = output(
        Command::new(&rustc).arg("-vV"),
        &format!("guest compiler {}", rustc.display()),
    )
    .await?;
    let sysroot = PathBuf::from(
        output(
            Command::new(&rustc).args(["--print", "sysroot"]),
            "guest compiler sysroot",
        )
        .await?
        .trim(),
    );
    let rustc = if let Some(selected) = override_rustc {
        let selected = PathBuf::from(selected);
        let override_version = output(
            Command::new(&selected).arg("-vV"),
            &format!("RUSTC override {}", selected.display()),
        )
        .await?;
        if override_version != version {
            return Err(rejected(format!(
                "RUSTC override {} is incompatible with pinned guest compiler {} ({channel}): override {}pinned {}",
                selected.display(),
                rustc.display(),
                override_version,
                version,
            )));
        }
        selected
    } else {
        rustc
    };
    Ok(GuestToolchain {
        channel,
        rustc,
        cargo,
        sysroot,
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[tokio::test]
    async fn default_guest_compiler_uses_repository_pin() {
        let toolchain = resolve(&root(), None).await.unwrap();
        let document: toml::Value = toml::from_str(
            &std::fs::read_to_string(root().join("tools/hash-rustc/rust-toolchain.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            toolchain.channel,
            document["toolchain"]["channel"].as_str().unwrap()
        );
        assert!(toolchain.rustc.is_absolute());
        assert!(toolchain.cargo.is_absolute());
        assert!(toolchain.sysroot.join("bin/rustc").is_file());
        let selected = output(Command::new(&toolchain.rustc).arg("-vV"), "test compiler")
            .await
            .unwrap();
        assert_eq!(selected, toolchain.version);
    }

    #[tokio::test]
    async fn explicit_matching_guest_compiler_is_accepted() {
        let pinned = resolve(&root(), None).await.unwrap();
        let selected = resolve(&root(), Some(pinned.rustc.clone().into_os_string()))
            .await
            .unwrap();
        assert_eq!(selected.rustc, pinned.rustc);
        assert_eq!(selected.version, pinned.version);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_mismatched_guest_compiler_names_override_and_pin() {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("loom-toolchain-override-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let compiler = directory.join("incompatible-rustc");
        std::fs::write(
            &compiler,
            "#!/bin/sh\nprintf '%s\\n' 'rustc incompatible-test-compiler'\n",
        )
        .unwrap();
        std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o755)).unwrap();
        let pinned = resolve(&root(), None).await.unwrap();
        let error = resolve(&root(), Some(compiler.clone().into_os_string()))
            .await
            .err()
            .expect("mismatched compiler accepted")
            .to_string();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(error.contains(compiler.to_str().unwrap()), "{error}");
        assert!(error.contains(&pinned.channel), "{error}");
        assert!(error.contains("incompatible"), "{error}");
    }
}
