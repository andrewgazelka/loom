//! One Linux VM per runner process. Native VM state never enters loomd.
#[cfg(target_os = "linux")]
mod krun;
#[cfg(target_os = "linux")]
mod rootfs;

#[cfg(target_os = "linux")]
fn run() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use std::path::PathBuf;
    let mut config = None;
    let mut library = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        let destination = if argument == "--config" {
            &mut config
        } else if argument == "--library" {
            &mut library
        } else {
            anyhow::bail!("unknown runner argument: {}", argument.to_string_lossy());
        };
        ensure!(
            destination.is_none(),
            "duplicate runner argument: {}",
            argument.to_string_lossy()
        );
        *destination = Some(PathBuf::from(
            args.next().context("runner option requires a path")?,
        ));
    }
    let config = config.context("--config PATH is required")?;
    let library = library.context("--library PATH is required")?;
    ensure!(
        config.is_absolute() && library.is_absolute(),
        "runner config and library paths must be absolute"
    );
    ensure!(
        library.is_file(),
        "libkrun library does not exist: {}",
        library.display()
    );
    let bytes = std::fs::read(&config)
        .with_context(|| format!("read launch config {}", config.display()))?;
    ensure!(bytes.len() <= 1024 * 1024, "VM launch config exceeds 1 MiB");
    let launch: loom_proto::VmLaunch =
        serde_json::from_slice(&bytes).context("invalid VM launch config")?;
    launch.spec.validate().map_err(anyhow::Error::msg)?;
    let root = rootfs::prepare(&launch.source_root, &launch.guest_root)?;
    rootfs::validate_command(&root, &launch.spec.command, &launch.spec.cwd)?;
    krun::enter(&library, &root, &launch.spec)
}

fn main() {
    #[cfg(target_os = "linux")]
    if let Err(error) = run() {
        eprintln!("loom-vm-runner: {error:#}");
        std::process::exit(1);
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("loom-vm-runner requires Linux with KVM; this platform is unsupported");
        std::process::exit(1);
    }
}
