//! `hash-rustc --loom-serve`: one process answers many compile requests over the
//! socket it was given as stdin, each with its own working directory,
//! environment and captured diagnostics, and exits when the parent closes it.
use std::io::{BufRead, BufReader, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

const DRIVER: &str = env!("CARGO_BIN_EXE_hash-rustc");

struct Server {
    child: std::process::Child,
    requests: UnixStream,
    replies: BufReader<UnixStream>,
}

impl Server {
    fn start() -> Self {
        let (requests, child_end) = UnixStream::pair().unwrap();
        let child = Command::new(DRIVER)
            .arg("--loom-serve")
            .stdin(Stdio::from(OwnedFd::from(child_end)))
            .stdout(Stdio::null())
            .spawn()
            .expect("start the server");
        let mut replies = BufReader::new(requests.try_clone().unwrap());
        let mut ready = String::new();
        replies.read_line(&mut ready).unwrap();
        assert_eq!(ready.trim(), "ready");
        Self {
            child,
            requests,
            replies,
        }
    }

    fn send(&mut self, request: &serde_json::Value) -> serde_json::Value {
        writeln!(self.requests, "{request}").unwrap();
        let mut reply = String::new();
        self.replies.read_line(&mut reply).unwrap();
        serde_json::from_str(&reply).unwrap()
    }

    fn compile(
        &mut self,
        directory: &std::path::Path,
        source: &str,
        env: &[(&str, &str)],
    ) -> serde_json::Value {
        std::fs::write(directory.join("lib.rs"), source).unwrap();
        let mut environment = serde_json::Map::new();
        environment.insert("PATH".into(), std::env::var("PATH").unwrap().into());
        for (name, value) in env {
            environment.insert((*name).into(), (*value).into());
        }
        self.send(&serde_json::json!({
            "cwd": directory,
            "env": environment,
            "args": [DRIVER, "--crate-name", "served", "--edition=2024", "--crate-type=lib",
                     "--emit=metadata", "--out-dir", directory.join("out"), "lib.rs"],
        }))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn one_server_answers_successive_compiles_and_reports_errors_per_request() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("out")).unwrap();
    let mut server = Server::start();

    let first = server.compile(directory.path(), "pub fn one() -> u32 { 1 }", &[]);
    assert_eq!(first["success"], true, "{first}");
    let metadata = directory.path().join("out/libserved.rmeta");
    assert!(metadata.exists());
    let first_len = std::fs::metadata(&metadata).unwrap().len();

    // A syntax error fails that request, with rustc's diagnostic in its own reply.
    let broken = server.compile(directory.path(), "pub fn broken( {", &[]);
    assert_eq!(broken["success"], false);
    assert!(
        broken["stderr"].as_str().unwrap().contains("error"),
        "{broken}"
    );

    // The next request is unaffected by the failure and by the previous stderr.
    let again = server.compile(
        directory.path(),
        "pub fn one() -> u32 { 1 }\npub fn two() -> u32 { 2 }",
        &[],
    );
    assert_eq!(again["success"], true, "{again}");
    assert_eq!(again["stderr"], "");
    assert_ne!(std::fs::metadata(&metadata).unwrap().len(), first_len);
}

#[test]
fn each_request_sees_exactly_its_own_environment() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("out")).unwrap();
    let mut server = Server::start();
    // Compile-time environment reads: a variable given to one request must be
    // gone from the next, and a request that has it must see its value.
    let sees_it = "const _: () = assert!(option_env!(\"SERVE_MARK\").unwrap().as_bytes()[0] == b'a');";
    let sees_none = "const _: () = assert!(option_env!(\"SERVE_MARK\").is_none());";
    let with = server.compile(directory.path(), sees_it, &[("SERVE_MARK", "a")]);
    assert_eq!(with["success"], true, "{with}");
    let without = server.compile(directory.path(), sees_none, &[]);
    assert_eq!(without["success"], true, "{without}");
    let wrong = server.compile(directory.path(), sees_it, &[]);
    assert_eq!(wrong["success"], false, "an absent variable must not satisfy the check");
}

#[test]
fn an_invalid_environment_entry_is_an_error_reply_not_a_dead_server() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("out")).unwrap();
    let mut server = Server::start();
    let bad = server.compile(directory.path(), "pub fn a() {}", &[("A=B", "x")]);
    assert_eq!(bad["success"], false);
    assert!(bad["stderr"].as_str().unwrap().contains("invalid environment entry"), "{bad}");
    let good = server.compile(directory.path(), "pub fn a() {}", &[]);
    assert_eq!(good["success"], true, "{good}");
}

#[test]
fn the_server_exits_when_its_parent_closes_the_socket() {
    let mut server = Server::start();
    // Dropping both ends of the parent's side closes the pair.
    let Server {
        child,
        requests,
        replies,
    } = &mut server;
    requests.shutdown(std::net::Shutdown::Both).unwrap();
    drop(replies.get_ref().shutdown(std::net::Shutdown::Both));
    let status = child.wait().unwrap();
    assert!(status.success());
}
