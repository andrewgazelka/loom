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
    let pin_path = root.join("tools/hash-rustc/rust-toolchain.toml");
    let pin: toml::Value = toml::from_str(&std::fs::read_to_string(&pin_path).unwrap()).unwrap();
    let channel = pin["toolchain"]["channel"]
        .as_str()
        .expect("driver toolchain pin must name channel");
    let rustc = Command::new("rustup")
        .args(["which", "--toolchain", channel, "rustc"])
        .output()
        .expect("resolve pinned guest rustc using rustup");
    assert!(
        rustc.status.success(),
        "{}: {}",
        pin_path.display(),
        String::from_utf8_lossy(&rustc.stderr)
    );
    let rustc = String::from_utf8(rustc.stdout).unwrap();
    let owner = cache_owner(&root);
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, test)
        .env("RUSTUP_TOOLCHAIN", channel)
        .env("RUSTC", rustc.trim())
        .env("LOOM_COMPILER_CACHE_OWNER", owner)
        .output()
        .expect("run pinned guest test subprocess");
    assert!(
        output.status.success(),
        "guest workflow {test} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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
