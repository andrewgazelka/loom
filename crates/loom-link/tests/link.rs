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
    fs::write(directory.join("version"), "1\n").unwrap();
    make_fifo(&pipe);
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
        // The directory may already be gone (a test removing it on purpose).
        if fs::write(&temporary, format!("{code}\n")).is_ok() {
            let _ = fs::rename(&temporary, &status);
        }
    })
}

fn make_fifo(pipe: &Path) {
    let c_pipe = CString::new(pipe.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_pipe.as_ptr(), 0o600) }, 0);
}

fn script(path: &Path, body: &str) -> PathBuf {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    path.to_owned()
}

#[test]
fn an_armed_link_hands_over_its_arguments_replays_the_output_and_returns_the_exit_code() {
    let directory = tempfile::tempdir().unwrap();
    let lld = fake_lld(directory.path(), 3);
    let waiter = arm(directory.path(), &lld);
    let output = Command::new(LINK)
        .env("LOOM_LINK_DIR", directory.path())
        .args([
            "--no-entry",
            "path with space.o",
            "quote\"d",
            "back\\slash",
            "-o",
            "out.wasm",
        ])
        .output()
        .unwrap();
    waiter.join().unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "linker-out\n");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "linker-err\n");
    let response = fs::read_to_string(directory.path().join("response")).unwrap();
    assert_eq!(
        response,
        "\"--no-entry\"\n\"path with space.o\"\n\"quote\\\"d\"\n\"back\\\\slash\"\n\"-o\"\n\"out.wasm\"\n"
    );
    assert!(
        !directory.path().join("args").exists(),
        "the pipe is used up"
    );
    // A second link in the same request finds no pipe and runs the real linker directly.
    let again = Command::new(LINK)
        .env("LOOM_LINK_DIR", directory.path())
        .env("LOOM_RUST_LLD", &lld)
        .args(["second"])
        .output()
        .unwrap();
    assert_eq!(again.status.code(), Some(3));
    assert_eq!(
        fs::read_to_string(directory.path().join("argv"))
            .unwrap()
            .trim(),
        "-flavor wasm second"
    );
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
fn a_linker_that_died_before_reading_its_arguments_is_replaced_by_a_direct_link() {
    let server = tempfile::tempdir().unwrap();
    let dead = script(
        &server.path().join("dead-lld"),
        "echo no-such-flag >&2\nexit 9",
    );
    let waiter = arm(server.path(), &dead);
    waiter.join().unwrap();
    // The arguments were never consumed, so the real linker gets them: its verdict stands, not the dead one's.
    let tools = tempfile::tempdir().unwrap();
    let direct = fake_lld(tools.path(), 4);
    let started = std::time::Instant::now();
    let output = Command::new(LINK)
        .env("LOOM_LINK_DIR", server.path())
        .env("LOOM_RUST_LLD", &direct)
        .arg("x.o")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("no-such-flag"));
    assert_eq!(
        fs::read_to_string(tools.path().join("argv"))
            .unwrap()
            .trim(),
        "-flavor wasm x.o"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn a_linker_that_closes_the_pipe_while_the_arguments_are_written_is_replaced_by_a_direct_link() {
    let server = tempfile::tempdir().unwrap();
    // Opens the response file, keeps it a moment without reading, then dies: a large write blocks
    // until the reader is gone and then fails with EPIPE.
    let closing = script(
        &server.path().join("closing-lld"),
        "exec 3< \"${3#@}\"\nsleep 0.3\nexit 9",
    );
    let waiter = arm(server.path(), &closing);
    let tools = tempfile::tempdir().unwrap();
    let direct = fake_lld(tools.path(), 0);
    let big = "x".repeat(256 * 1024);
    let output = Command::new(LINK)
        .env("LOOM_LINK_DIR", server.path())
        .env("LOOM_RUST_LLD", &direct)
        .args([big.as_str(), "y.o"])
        .output()
        .unwrap();
    waiter.join().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let argv = fs::read_to_string(tools.path().join("argv")).unwrap();
    assert!(
        argv.starts_with("-flavor wasm xxxx") && argv.trim_end().ends_with("y.o"),
        "the direct linker received the arguments"
    );
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
    assert_eq!(
        fs::read_to_string(directory.path().join("argv"))
            .unwrap()
            .trim(),
        "-flavor wasm --gc-sections a.o"
    );
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
    assert_eq!(
        fs::read_to_string(plain.path().join("argv"))
            .unwrap()
            .trim(),
        "-flavor wasm --no-entry"
    );
}

#[test]
fn a_server_of_another_protocol_or_an_empty_argument_is_linked_directly_and_the_waiting_linker_is_left_alone()
 {
    let server = tempfile::tempdir().unwrap();
    make_fifo(&server.path().join("args"));
    let tools = tempfile::tempdir().unwrap();
    let recorder = script(
        &tools.path().join("recorder"),
        &format!(
            "printf '[%s]\\n' \"$@\" > \"{}/argv\"",
            tools.path().display()
        ),
    );
    let link = |arguments: &[&str]| {
        Command::new(LINK)
            .env("LOOM_LINK_DIR", server.path())
            .env("LOOM_RUST_LLD", &recorder)
            .args(arguments)
            .output()
            .unwrap()
    };
    // No `version` file, then a different version: the server is not one this program understands.
    assert!(link(&["a.o"]).status.success());
    assert_eq!(
        fs::read_to_string(tools.path().join("argv")).unwrap(),
        "[-flavor]\n[wasm]\n[a.o]\n"
    );
    fs::write(server.path().join("version"), "2\n").unwrap();
    assert!(link(&["b.o"]).status.success());
    assert_eq!(
        fs::read_to_string(tools.path().join("argv")).unwrap(),
        "[-flavor]\n[wasm]\n[b.o]\n"
    );
    // The right version, but an empty argument: a response file cannot carry it (LLVM's tokenizer
    // drops an empty token), so the command line does.
    fs::write(server.path().join("version"), "1\n").unwrap();
    assert!(link(&["a.o", "", "b.o"]).status.success());
    assert_eq!(
        fs::read_to_string(tools.path().join("argv")).unwrap(),
        "[-flavor]\n[wasm]\n[a.o]\n[]\n[b.o]\n"
    );
    assert!(
        server.path().join("args").exists(),
        "the pipe was never touched"
    );
}

/// Wait for `child` for at most `limit`; a hang fails the test instead of the run.
fn exits_within(
    child: &mut std::process::Child,
    limit: std::time::Duration,
) -> std::process::ExitStatus {
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            panic!("loom-link was still waiting after {limit:?}");
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn a_link_stops_waiting_when_the_server_removes_its_directory() {
    let server = tempfile::tempdir().unwrap();
    // Reads all its arguments, then never finishes: no status will come.
    let stuck = script(
        &server.path().join("stuck-lld"),
        "cat \"${3#@}\" > /dev/null\nsleep 5",
    );
    let _waiter = arm(server.path(), &stuck);
    let mut child = Command::new(LINK)
        .env("LOOM_LINK_DIR", server.path())
        .arg("x.o")
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(std::time::Duration::from_millis(500));
    fs::remove_dir_all(server.path()).unwrap();
    let status = exits_within(&mut child, std::time::Duration::from_secs(10));
    assert_eq!(status.code(), Some(1));
    let mut message = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut message).unwrap();
    assert!(
        message.contains("removed the linker directory"),
        "{message}"
    );
}

#[test]
fn a_link_stops_waiting_when_the_process_that_ran_it_is_gone() {
    let server = tempfile::tempdir().unwrap();
    make_fifo(&server.path().join("args"));
    fs::write(server.path().join("version"), "1\n").unwrap();
    let errors = server.path().join("errors");
    // The shell starts loom-link in the background (an armed pipe with no lld behind it, so it
    // polls for a reader) and exits: loom-link is orphaned.
    Command::new("sh")
        .arg("-c")
        .arg("\"$0\" x.o 2> \"$1\" & sleep 0.4")
        .arg(LINK)
        .arg(&errors)
        .env("LOOM_LINK_DIR", server.path())
        .status()
        .unwrap();
    let started = std::time::Instant::now();
    loop {
        if fs::read_to_string(&errors).is_ok_and(|text| text.contains("is gone")) {
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "an orphaned link kept waiting"
        );
        thread::sleep(std::time::Duration::from_millis(20));
    }
}
