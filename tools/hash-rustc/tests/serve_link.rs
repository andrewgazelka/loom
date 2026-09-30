//! The pre-armed linker's lifecycle in `hash-rustc --loom-serve` (`serve.rs`, `Armed`): a request
//! that never links kills the waiting lld and removes its directory at once, and a new server
//! reaps what a dead one left behind. The linking itself is tested in `crates/loom-link`.
use std::io::{BufRead, BufReader, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const DRIVER: &str = env!("CARGO_BIN_EXE_hash-rustc");

/// A server whose link directories live in `temp`.
struct Server {
    child: Child,
    requests: UnixStream,
    replies: BufReader<UnixStream>,
}

impl Server {
    fn start(temp: &Path) -> Self {
        let (requests, child_end) = UnixStream::pair().unwrap();
        let child = Command::new(DRIVER)
            .arg("--loom-serve")
            .env("TMPDIR", temp)
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
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn script(path: &Path, body: &str) -> PathBuf {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_owned()
}

fn link_directories(temp: &Path) -> Vec<String> {
    std::fs::read_dir(temp)
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("hash-rustc-link-"))
        .collect()
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[test]
fn a_request_that_never_links_kills_the_waiting_lld_and_removes_its_directory() {
    let temp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let pid_file = temp.path().join("lld.pid");
    // Records its pid, then waits on its response file the way the real lld does.
    let lld = script(
        &temp.path().join("rust-lld"),
        &format!("echo $$ > \"{}\"\nexec sleep 30", pid_file.display()),
    );
    let mut server = Server::start(temp.path());
    std::fs::write(work.path().join("lib.rs"), "pub fn one() -> u32 { 1 }").unwrap();
    let request = serde_json::json!({
        "cwd": work.path(),
        "env": {"PATH": std::env::var("PATH").unwrap(), "LOOM_LINK_ARM": lld},
        "args": [DRIVER, "--crate-name", "served", "--edition=2024", "--crate-type=lib",
                 "--emit=metadata", "--out-dir", work.path(), "lib.rs"],
    });
    writeln!(server.requests, "{request}").unwrap();
    let mut reply = String::new();
    server.replies.read_line(&mut reply).unwrap();
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["success"], true, "{reply}");
    let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    assert!(!alive(pid), "the waiting lld outlived its request");
    assert!(link_directories(temp.path()).is_empty(), "{:?}", link_directories(temp.path()));
}

#[test]
fn a_new_server_reaps_the_lld_and_directory_a_dead_server_left_behind() {
    let temp = tempfile::tempdir().unwrap();
    let lld = script(&temp.path().join("rust-lld"), "sleep 20");
    let dead_server = {
        let mut finished = Command::new("true").spawn().unwrap();
        let pid = finished.id();
        finished.wait().unwrap();
        pid
    };
    // What a dead server leaves: its directory and an lld blocked on the pipe (the fake is started
    // as the real one is, so its command line names the lld and the directory).
    let orphan_directory = temp.path().join(format!("hash-rustc-link-{dead_server}-0"));
    std::fs::create_dir(&orphan_directory).unwrap();
    let mut orphan = Command::new(&lld)
        .args(["-flavor", "wasm"])
        .arg(format!("@{}", orphan_directory.join("args").display()))
        .spawn()
        .unwrap();
    std::fs::write(orphan_directory.join("pid"), orphan.id().to_string()).unwrap();
    // A dead server whose pid file names an unrelated process (a recycled pid): the directory goes,
    // the process is not touched.
    let recycled_directory = temp.path().join(format!("hash-rustc-link-{dead_server}-1"));
    std::fs::create_dir(&recycled_directory).unwrap();
    let mut unrelated = Command::new("sleep").arg("20").spawn().unwrap();
    std::fs::write(recycled_directory.join("pid"), unrelated.id().to_string()).unwrap();
    // A live server's directory (this test process is its owner) is left alone, lld and all.
    let live_directory = temp.path().join(format!("hash-rustc-link-{}-0", std::process::id()));
    std::fs::create_dir(&live_directory).unwrap();
    let mut live = Command::new(&lld)
        .args(["-flavor", "wasm"])
        .arg(format!("@{}", live_directory.join("args").display()))
        .spawn()
        .unwrap();
    std::fs::write(live_directory.join("pid"), live.id().to_string()).unwrap();

    let _server = Server::start(temp.path());

    assert!(!orphan_directory.exists() && !recycled_directory.exists());
    assert_eq!(
        orphan.wait().unwrap().signal(),
        Some(9),
        "the orphaned lld is killed before the server says ready"
    );
    assert!(unrelated.try_wait().unwrap().is_none(), "a pid that is not the lld is not signalled");
    assert!(live_directory.exists() && live.try_wait().unwrap().is_none());
    let _ = unrelated.kill();
    let _ = live.kill();
    let _ = unrelated.wait();
    let _ = live.wait();
}
