#[path = "support/guest.rs"]
mod guest;

use loom_api::Service;
use loom_build::Builder;
use loom_proto::{CommandRequest, Lang};
use loom_store::Store;
use serde_json::json;
use std::path::{Path, PathBuf};

const TEST: &str = "prebuilt_driver_add_and_run_without_tool_sources";
const RUNTIME_ROOT: &str = "LOOM_PREBUILT_TEST_RUNTIME_ROOT";

#[test]
fn prebuilt_driver_add_and_run_without_tool_sources() {
    if let Some(root) = std::env::var_os(RUNTIME_ROOT) {
        let error = std::process::Command::new("rustup")
            .arg("--version")
            .output()
            .expect_err("runtime PATH must not contain rustup");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(runtime_workflow(Path::new(&root)))
            .unwrap();
        return;
    }
    guest::run(TEST, || async { workflow().await.unwrap() });
}

async fn workflow() -> anyhow::Result<()> {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let preparation = Store::memory()?;
    let builder = Builder::new(repository.clone(), preparation.clone());
    builder.preflight().await?;
    let toolchain = loom_build::resolve_guest_toolchain(&repository).await?;
    let version_hash = preparation.put("toolchain-version", toolchain.version.as_bytes())?;
    let driver = builder
        .with_cache_exclusive(|cache| {
            cache
                .join("hash-rustc")
                .join(version_hash)
                .join("release/hash-rustc")
        })
        .await
        .canonicalize()?;
    assert!(driver.is_file(), "prebuilt driver {}", driver.display());

    let runtime = tempfile::tempdir()?;
    for name in ["Cargo.toml", "Cargo.lock", "crates", "rustc"] {
        std::os::unix::fs::symlink(repository.join(name), runtime.path().join(name))?;
    }
    assert!(!runtime.path().join("tools").exists());
    let path = std::env::join_paths([
        toolchain.sysroot.join("bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])?;
    let output = tokio::process::Command::new(std::env::current_exe()?)
        .args(["--exact", TEST, "--nocapture"])
        .env(RUNTIME_ROOT, runtime.path())
        .env("LOOM_HASH_RUSTC", &driver)
        .env("PATH", path)
        .env_remove("RUSTC")
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .await?;
    assert!(
        output.status.success(),
        "prebuilt runtime without rustup failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!runtime.path().join("tools").exists());
    Ok(())
}

async fn runtime_workflow(root: &Path) -> anyhow::Result<()> {
    assert!(!root.join("tools").exists());
    let service = Service::new(Store::memory()?, root.to_owned(), vec![Lang::Rust])?;
    let added = service
        .command(CommandRequest {
            session: None,
            command: "add".into(),
            args: json!({"name":"values", "source":"pub fn values() -> i32 { 42 }"}),
        })
        .await;
    assert!(added.ok, "{added:?}");
    let run = service
        .command(CommandRequest {
            session: None,
            command: "run".into(),
            args: json!({"target":added.result["hash"]}),
        })
        .await;
    assert!(run.ok, "{run:?}");
    assert_eq!(run.result["output"], 42);
    assert!(!root.join("tools").exists());
    Ok(())
}
