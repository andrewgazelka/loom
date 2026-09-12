use loom_api::{Authorizer, Service};
use loom_proto::{Lang, Response};
use loom_store::Store;
use std::{path::PathBuf, sync::Arc};

struct TransportRegistry;

#[async_trait::async_trait]
impl loom_actor::Registry for TransportRegistry {
    async fn resolve(&self, reference: &str) -> anyhow::Result<Arc<dyn loom_actor::Behavior>> {
        anyhow::ensure!(reference == "counter-v1", "unknown behavior {reference}");
        Ok(Arc::new(loom_actor::builtin::Counter::plain()))
    }

    async fn behaviors(&self) -> anyhow::Result<Vec<loom_actor::builtin::BehaviorInfo>> {
        Ok(vec![loom_actor::builtin::BehaviorInfo {
            hash: "counter-v1".into(),
            description: "Transport test counter.".into(),
        }])
    }
}

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}
impl Server {
    async fn start() -> Self {
        Self::with_registry(false).await
    }
    async fn with_registry(guest: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::memory().unwrap();
        let registry: Arc<dyn loom_actor::Registry> = if guest {
            Arc::new(loom_behavior::StoreRegistry::new(store.clone()))
        } else {
            Arc::new(TransportRegistry)
        };
        let node = loom_actor::Node::new(
            directory.path().join("actors"),
            registry,
            Arc::new(loom_actor::DefaultEffects),
            loom_actor::Config::default(),
        )
        .await
        .unwrap();
        let service = Arc::new(
            Service::new(
                store,
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
                vec![Lang::Rust],
            )
            .unwrap()
            .with_actors(node),
        );
        let router = loom_api::router(service, Authorizer::single("test".into()).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            url,
            task,
            _directory: directory,
        }
    }
    async fn invoke(&self, arguments: &[&str]) -> Response {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--url", &self.url, "--token", "test"])
            .args(arguments)
            .output()
            .await
            .unwrap();
        let response: Response = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{arguments:?}: {error}; stderr={}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(output.status.success(), response.ok, "{arguments:?}");
        response
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn actor_commands_reach_shared_service() {
    let server = Server::start().await;
    let spawned = server.invoke(&["spawn", "counter-v1"]).await;
    assert!(spawned.ok, "{spawned:?}");
    let id = spawned.result["id"].as_str().unwrap();
    let sent = server
        .invoke(&["send", id, "{\"n\":1}", "--key", "delivery"])
        .await;
    assert!(sent.ok, "{sent:?}");
    assert_eq!(sent.result["cursor"], 1);
    for arguments in [
        vec!["tree"],
        vec!["info", id],
        vec!["lineage", id],
        vec!["actors"],
    ] {
        let response = server.invoke(&arguments).await;
        assert!(response.ok, "{arguments:?}: {response:?}");
    }
    for arguments in [
        vec!["dead_letters", id],
        vec!["sql", id, "SELECT 1 AS value"],
        vec!["register", "transport-counter", id],
        vec!["whereis", "transport-counter"],
        vec!["members", "absent-group"],
        vec!["behaviors"],
    ] {
        let response = server.invoke(&arguments).await;
        assert!(response.ok, "{arguments:?}: {response:?}");
    }
    let validated = server.invoke(&["validate", id, "counter-v1", "1"]).await;
    assert!(validated.ok, "{validated:?}");
    assert!(
        validated.result["verdict"].get("Matched").is_some(),
        "{validated:?}"
    );
    let promoted = server
        .invoke(&[
            "promote",
            id,
            "counter-v1",
            "--rationale",
            "verified",
            "--author",
            "transport-test",
        ])
        .await;
    assert!(promoted.ok, "{promoted:?}");
    let forked = server.invoke(&["fork", id, "0"]).await;
    assert!(forked.ok, "{forked:?}");
    assert!(server.invoke(&["stop", id, "transport-test"]).await.ok);
    assert!(server.invoke(&["restart", id, "resume"]).await.ok);
}

#[tokio::test]
async fn repl_uses_same_commands_and_preserves_json() {
    use tokio::io::AsyncWriteExt;
    let server = Server::start().await;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_loom"))
        .args(["--url", &server.url, "--token", "test"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"find 'two words'\nrun missing '{\"value\": 1}'\nquit\n")
        .await
        .unwrap();
    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("\"ok\": true"), "{stdout}");
    assert!(stdout.contains("\"ok\": false"), "{stdout}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("invalid JSON"), "{stderr}");
}

#[test]
fn retired_vocabulary_is_rejected() {
    for arguments in [
        ["--eval", "1"],
        ["--define", "{}"],
        ["crate", "add"],
        ["upgrade", "old"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(["--token", "test"])
            .args(arguments)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let response: loom_proto::Response = serde_json::from_slice(&output.stdout).unwrap();
        assert!(!response.ok);
        assert!(
            response.result["error"]
                .as_str()
                .unwrap()
                .contains("error:")
        );
    }
}

#[test]
fn real_guest_definition_commands() {
    const CHILD: &str = "LOOM_CLI_GUEST_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(definition_commands_reach_shared_service());
        return;
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let pin_path = root.join("tools/hash-rustc/rust-toolchain.toml");
    let pin = std::fs::read_to_string(&pin_path).unwrap();
    let channel = pin
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "channel").then(|| value.trim().trim_matches('"'))
        })
        .expect("driver toolchain pin must name channel");
    let rustc = std::process::Command::new("rustup")
        .args(["which", "--toolchain", channel, "rustc"])
        .output()
        .expect("resolve pinned guest rustc using rustup");
    assert!(
        rustc.status.success(),
        "{}: {}",
        pin_path.display(),
        String::from_utf8_lossy(&rustc.stderr)
    );
    let rustc_path = String::from_utf8(rustc.stdout).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "real_guest_definition_commands", "--nocapture"])
        .env(CHILD, "1")
        .env("RUSTUP_TOOLCHAIN", channel)
        .env("RUSTC", rustc_path.trim())
        .env("LOOM_COMPILER_CACHE_OWNER", env!("CARGO_BIN_EXE_loom"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "guest workflow failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn definition_commands_reach_shared_service() {
    let server = Server::with_registry(true).await;
    let file = tempfile::NamedTempFile::new().unwrap();
    let path = file.path().to_str().unwrap();
    let first = "pub fn main() -> i32 { let value = 41; value }";
    std::fs::write(file.path(), first).unwrap();
    let added = server.invoke(&["add", path, "--name", "answer"]).await;
    assert!(added.ok, "{added:?}");
    let old = added.result["hash"].as_str().unwrap();
    for key in ["behavior_hash", "wasm_hash"] {
        assert_eq!(added.result[key].as_str().unwrap().len(), 64);
    }
    std::fs::remove_file(file.path()).unwrap();
    let viewed = server.invoke(&["view", old]).await;
    assert!(viewed.ok, "{viewed:?}");
    assert!(viewed.result["source"].as_str().unwrap().contains("41"));
    assert!(!viewed.result["items"].as_object().unwrap().is_empty());
    std::fs::write(file.path(), "pub fn main() -> i32 { answer::main() }").unwrap();
    let pins = serde_json::json!({"answer":old}).to_string();
    let caller = server
        .invoke(&["add", path, "--name", "caller", "--deps", &pins])
        .await;
    assert!(caller.ok, "{caller:?}");
    std::fs::write(file.path(), first.replace("41", "42")).unwrap();
    let updated = server.invoke(&["update", "answer", path]).await;
    assert!(updated.ok, "{updated:?}");
    let new = updated.result["hash"].as_str().unwrap();
    assert_ne!(old, new);
    let run = server.invoke(&["run", "answer"]).await;
    assert!(run.ok, "{run:?}");
    assert_eq!(run.result["output"], 42);
    assert!(run.result["effects"].is_array());
    assert_eq!(server.invoke(&["run", old]).await.result["output"], 41);
    assert_eq!(server.invoke(&["run", "caller"]).await.result["output"], 41);
    let history = server.invoke(&["history", "answer"]).await;
    assert!(history.ok, "{history:?}");
    assert_eq!(history.result.as_array().unwrap().len(), 2);
    let diff = server.invoke(&["diff", old, new]).await;
    assert!(diff.ok, "{diff:?}");
    assert!(!diff.result["changed"].as_array().unwrap().is_empty());
    let found = server.invoke(&["find", "answer"]).await;
    assert!(found.ok, "{found:?}");
    assert!(!found.result.as_array().unwrap().is_empty());
    std::fs::write(file.path(), "pub fn main() -> i32 { answer::main() + 1 }").unwrap();
    let revised_caller = server.invoke(&["update", "caller", path]).await;
    assert!(revised_caller.ok, "{revised_caller:?}");
    assert_eq!(server.invoke(&["run", "caller"]).await.result["output"], 42);
    let dependents = server.invoke(&["dependents", old]).await;
    assert!(dependents.ok, "{dependents:?}");
    let dependent_hashes = dependents.result.as_array().unwrap();
    assert_eq!(dependent_hashes.len(), 2);
    assert!(dependent_hashes.contains(&caller.result["hash"]));
    assert!(dependent_hashes.contains(&revised_caller.result["hash"]));
}
