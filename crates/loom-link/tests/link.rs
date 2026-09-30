//! `loom-link` against a fake lld, playing the server's part (fifo, waiter, status file).
use std::{
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

const LINK: &str = env!("CARGO_BIN_EXE_loom-link");

/// A stand-in for `rust-lld`: records the arguments it received and, when given a response file
/// (`@path`), the file's contents; writes to both streams; exits with `exit`.
fn fake_lld(directory: &Path, exit: i32) -> PathBuf {
    let path = directory.join("fake-lld");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\necho \"$@\" > \"{d}/argv\"\nfor a in \"$@\"; do case \"$a\" in @*) cat \"${{a#@}}\" > \"{d}/response\";; esac; done\necho linker-out\necho linker-err >&2\nexit {exit}\n",
            d = directory.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The server's part: a fifo, an lld reading it, and a waiter that records the exit status.
fn arm(directory: &Path, lld: &Path) -> thread::JoinHandle<()> {
    let pipe = directory.join("args");
    let c_pipe = CString::new(pipe.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_pipe.as_ptr(), 0o600) }, 0);
    let mut child = Command::new(lld)
        .arg("-flavor")
        .arg("wasm")
        .arg(format!("@{}", pipe.display()))
        .stdin(Stdio::null())
        .stdout(fs::File::create(directory.join("stdout")).unwrap())
        .stderr(fs::File::create(directory.join("stderr")).unwrap())
        .spawn()
        .unwrap();
    let status = directory.join("status");
    thread::spawn(move || {
        let code = child.wait().unwrap().code().unwrap_or(-1);
        let temporary = status.with_extension("tmp");
        fs::write(&temporary, format!("{code}\n")).unwrap();
        fs::rename(&temporary, &status).unwrap();
    })
}

#[test]
fn an_armed_link_hands_over_its_arguments_replays_the_output_and_returns_the_exit_code() {
    let directory = tempfile::tempdir().unwrap();
    let lld = fake_lld(directory.path(), 3);
    let waiter = arm(directory.path(), &lld);
    let output = Command::new(LINK)
        .env("LOOM_LINK_DIR", directory.path())
        .args(["--no-entry", "path with space.o", "quote\"d", "back\\slash", "-o", "out.wasm"])
        .output()
        .unwrap();
    waiter.join().unwrap();
    assert_eq!(output.status.code(), Some(3), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "linker-out\n");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "linker-err\n");
    let response = fs::read_to_string(directory.path().join("response")).unwrap();
    assert_eq!(
        response,
        "\"--no-entry\"\n\"path with space.o\"\n\"quote\\\"d\"\n\"back\\\\slash\"\n\"-o\"\n\"out.wasm\"\n"
    );
    assert!(!directory.path().join("args").exists(), "the pipe is used up");
    // A second link in the same request finds no pipe and runs the real linker directly.
    let again = Command::new(LINK)
        .env("LOOM_LINK_DIR", directory.path())
        .env("LOOM_RUST_LLD", &lld)
        .args(["second"])
        .output()
        .unwrap();
    assert_eq!(again.status.code(), Some(3));
    assert_eq!(fs::read_to_string(directory.path().join("argv")).unwrap().trim(), "-flavor wasm second");
}

#[test]
fn a_successful_armed_link_exits_zero() {
    let directory = tempfile::tempdir().unwrap();
    let lld = fake_lld(directory.path(), 0);
    let waiter = arm(directory.path(), &lld);
    let output = Command::new(LINK)
        .env("LOOM_LINK_DIR", directory.path())
        .arg("x.o")
        .output()
        .unwrap();
    waiter.join().unwrap();
    assert!(output.status.success());
}

#[test]
fn a_linker_that_died_before_reading_its_arguments_is_reported_not_waited_for() {
    let directory = tempfile::tempdir().unwrap();
    let dead = directory.path().join("dead-lld");
    fs::write(&dead, "#!/bin/sh\necho no-such-flag >&2\nexit 9\n").unwrap();
    fs::set_permissions(&dead, fs::Permissions::from_mode(0o755)).unwrap();
    let waiter = arm(directory.path(), &dead);
    waiter.join().unwrap();
    let started = std::time::Instant::now();
    let output = Command::new(LINK)
        .env("LOOM_LINK_DIR", directory.path())
        .arg("x.o")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(9));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no-such-flag"));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn without_an_armed_directory_it_runs_the_real_linker_directly() {
    let directory = tempfile::tempdir().unwrap();
    let lld = fake_lld(directory.path(), 0);
    let output = Command::new(LINK)
        .env_remove("LOOM_LINK_DIR")
        .env("LOOM_RUST_LLD", &lld)
        .args(["--gc-sections", "a.o"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(directory.path().join("argv")).unwrap().trim(), "-flavor wasm --gc-sections a.o");
    // A directory named but never armed behaves the same.
    let unarmed = tempfile::tempdir().unwrap();
    let output = Command::new(LINK)
        .env("LOOM_LINK_DIR", unarmed.path())
        .env("LOOM_RUST_LLD", &lld)
        .arg("b.o")
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
fn the_flavor_pair_rustc_prepends_is_not_forwarded_to_the_waiting_linker() {
    let directory = tempfile::tempdir().unwrap();
    let lld = fake_lld(directory.path(), 0);
    let waiter = arm(directory.path(), &lld);
    let output = Command::new(LINK)
        .env("LOOM_LINK_DIR", directory.path())
        .args(["-flavor", "wasm", "--no-entry", "a.o"])
        .output()
        .unwrap();
    waiter.join().unwrap();
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(directory.path().join("response")).unwrap(),
        "\"--no-entry\"\n\"a.o\"\n",
        "`-flavor` inside a response file would be an lld error"
    );
    // The direct path passes exactly one flavor pair too.
    let plain = tempfile::tempdir().unwrap();
    let lld = fake_lld(plain.path(), 0);
    let output = Command::new(LINK)
        .env_remove("LOOM_LINK_DIR")
        .env("LOOM_RUST_LLD", &lld)
        .args(["-flavor", "wasm", "--no-entry"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(plain.path().join("argv")).unwrap().trim(), "-flavor wasm --no-entry");
}
