use loom_api::{Authorizer, Service};
use loom_proto::{Lang, Response};
use loom_store::Store;
use std::{path::PathBuf, sync::Arc};

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}
impl Server {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let node = loom_actor::Node::new(
            directory.path().join("actors"),
            loom_actor::Registry::new(),
            Arc::new(loom_actor::DefaultEffects),
            loom_actor::Config::default(),
        )
        .await
        .unwrap();
        let service = Arc::new(
            Service::new(
                Store::memory().unwrap(),
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
async fn definition_commands_reach_shared_service() {
    let server = Server::start().await;
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), "not valid Rust").unwrap();
    let path = file.path().to_str().unwrap();
    let cases = [
        vec!["add", path, "--name", "counter"],
        vec!["view", "missing"],
        vec!["update", "missing", path],
        vec!["history", "missing"],
        vec!["diff", "missing", "other"],
        vec!["run", "missing", "{\"value\":1}"],
        vec!["dependents", "missing"],
    ];
    for arguments in cases {
        let response = server.invoke(&arguments).await;
        let error = response.result["error"].as_str().unwrap_or("");
        assert!(
            !error.contains("unknown command"),
            "{arguments:?}: {response:?}"
        );
        assert!(
            !error.contains("missing string argument"),
            "{arguments:?}: {response:?}"
        );
    }
    assert!(server.invoke(&["find", "absent"]).await.ok);
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
    let validated = server.invoke(&["validate", id, "counter-v1", "1"]).await;
    assert!(validated.ok, "{validated:?}");
    assert!(
        validated.result["verdict"].get("Matched").is_some(),
        "{validated:?}"
    );
    let promoted = server
        .invoke(&["promote", id, "counter-v1", "--rationale", "verified"])
        .await;
    assert!(promoted.ok, "{promoted:?}");
    let forked = server.invoke(&["fork", id, "0"]).await;
    assert!(forked.ok, "{forked:?}");
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
        assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));
    }
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn real_guest_definition_commands() {
    let server = Server::start().await;
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
