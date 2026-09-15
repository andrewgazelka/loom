use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run the real guest workflow in a child so compiler environment changes cannot
/// race other tests in this process.
pub fn run<F: Future<Output = ()>>(test: &str, workflow: impl FnOnce() -> F) {
    const CHILD: &str = "LOOM_API_GUEST_TEST_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        tokio::runtime::Runtime::new().unwrap().block_on(workflow());
        return;
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let owner = cache_owner(&root);
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, test)
        .env_remove("RUSTUP_TOOLCHAIN")
        .env("LOOM_COMPILER_CACHE_OWNER", owner);
    if let Some(driver) = std::env::var_os("LOOM_HASH_RUSTC") {
        // Packaged guests use this exact compiler/driver pair. Clearing RUSTC
        // would turn the production contract into an unrelated rustup lookup.
        assert!(
            Path::new(&driver).is_absolute(),
            "LOOM_HASH_RUSTC must be absolute"
        );
        let rustc = std::env::var_os("RUSTC")
            .expect("prebuilt LOOM_HASH_RUSTC requires an absolute RUSTC path");
        assert!(Path::new(&rustc).is_absolute(), "RUSTC must be absolute");
    } else {
        child.env_remove("RUSTC");
    }
    let output = child.output().expect("run pinned guest test subprocess");
    assert!(
        output.status.success(),
        "guest workflow {test} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
}

fn cache_owner(root: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os("LOOM_COMPILER_CACHE_OWNER") {
        return path.into();
    }
    // Reuse the production CLI's compiler-cache entrypoint. Cargo reports the
    // executable path, including custom target directories and host targets.
    let output = Command::new("cargo")
        .current_dir(root)
        .args([
            "build",
            "-p",
            "loom-cli",
            "--bin",
            "loom",
            "--message-format=json",
        ])
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("RUSTC")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .output()
        .expect("build the loom compiler-cache owner");
    assert!(
        output.status.success(),
        "building loom compiler-cache owner failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|message| {
            (message["reason"] == "compiler-artifact" && message["target"]["name"] == "loom")
                .then(|| message["executable"].as_str().map(PathBuf::from))
                .flatten()
        })
        .expect("cargo must report the loom executable path")
}
