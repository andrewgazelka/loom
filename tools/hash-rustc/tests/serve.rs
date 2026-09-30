//! `hash-rustc --loom-serve`: one process answers many compile requests, each
//! with its own working directory, environment and captured diagnostics, and
//! exits when its parent's pipe closes.
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

const DRIVER: &str = env!("CARGO_BIN_EXE_hash-rustc");

struct Server {
    child: std::process::Child,
    socket: std::path::PathBuf,
}

impl Server {
    fn start(directory: &std::path::Path) -> Self {
        let socket = directory.join("serve.sock");
        let mut child = Command::new(DRIVER)
            .arg("--loom-serve")
            .arg(&socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start the server");
        let mut ready = String::new();
        BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "ready");
        Self { child, socket }
    }

    fn compile(&self, directory: &std::path::Path, source: &str, marker: &str) -> serde_json::Value {
        std::fs::write(directory.join("lib.rs"), source).unwrap();
        let request = serde_json::json!({
            "cwd": directory,
            "env": {"HASH_RUSTC_SERVE_MARKER": marker, "PATH": std::env::var("PATH").unwrap()},
            "args": [DRIVER, "--crate-name", "served", "--edition=2024", "--crate-type=lib",
                     "--emit=metadata", "--out-dir", directory.join("out"), "lib.rs"],
        });
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        writeln!(stream, "{request}").unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).unwrap();
        serde_json::from_str(&reply).unwrap()
    }
}

#[test]
fn one_server_answers_successive_compiles_and_reports_errors_per_request() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("out")).unwrap();
    let server = Server::start(directory.path());

    let first = server.compile(directory.path(), "pub fn one() -> u32 { 1 }", "a");
    assert_eq!(first["success"], true, "{first}");
    let metadata = directory.path().join("out/libserved.rmeta");
    assert!(metadata.exists());
    let first_len = std::fs::metadata(&metadata).unwrap().len();

    // A syntax error fails that request, with rustc's diagnostic in its own reply.
    let broken = server.compile(directory.path(), "pub fn broken( {", "b");
    assert_eq!(broken["success"], false);
    assert!(
        broken["stderr"].as_str().unwrap().contains("error"),
        "{broken}"
    );

    // The next request is unaffected by the failure and by the previous stderr.
    let again = server.compile(
        directory.path(),
        "pub fn one() -> u32 { 1 }\npub fn two() -> u32 { 2 }",
        "c",
    );
    assert_eq!(again["success"], true, "{again}");
    assert_eq!(again["stderr"], "");
    assert_ne!(std::fs::metadata(&metadata).unwrap().len(), first_len);
    drop(server);
}

#[test]
fn the_server_exits_when_its_parent_pipe_closes() {
    let directory = tempfile::tempdir().unwrap();
    let mut server = Server::start(directory.path());
    drop(server.child.stdin.take());
    let status = server.child.wait().unwrap();
    assert!(status.success());
    let mut rest = String::new();
    server
        .child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut rest)
        .unwrap();
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
