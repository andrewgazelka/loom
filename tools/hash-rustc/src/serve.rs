//! `hash-rustc --loom-serve`: one long-lived compiler process.
//!
//! Starting rustc costs about 45 ms (loading `librustc_driver`) before it reads
//! a byte of source, and a small guest compiles in about 35 ms once it is
//! running, so a process per compile is most of a cell's latency. This mode
//! serves requests on its standard input, which the parent made one end of a
//! socket pair, and runs the same driver entry (`crate::compile`) in this
//! process: `rustc_driver::run_compiler` is re-entrant (100 back-to-back
//! compiles measured flat at 36 ms with a stable resident set).
//!
//! There is no named socket: the only two parties are the parent and this
//! process, so nothing on the filesystem for another user to connect to, no
//! path length limit and no stale file to clean up.
//!
//! A request is one line of JSON, `{"cwd", "env", "args"}`: the working
//! directory, the complete environment and the compiler arguments a process
//! would have been given. The reply is one line, `{"success", "stdout",
//! "stderr"}`. The process environment and the standard streams are per-request
//! global state, which is sound only because requests are served one at a time
//! and no compiler thread outlives its request.
//!
//! The server sends `ready` first and exits when the parent closes the socket,
//! so it never outlives the process that started it.
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

unsafe extern "C" {
    fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
    fn geteuid() -> u32;
    fn dup(fd: i32) -> i32;
    fn dup2(from: i32, to: i32) -> i32;
    fn close(fd: i32) -> i32;
}

#[derive(serde::Deserialize)]
struct Request {
    cwd: String,
    env: std::collections::BTreeMap<String, String>,
    args: Vec<String>,
}

#[derive(serde::Serialize)]
struct Reply {
    success: bool,
    stdout: String,
    stderr: String,
}

/// A standard stream redirected into an unlinked temporary file for one request.
struct Capture {
    target: i32,
    saved: i32,
    file: std::fs::File,
}

impl Capture {
    fn start(target: i32, label: &str) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "hash-rustc-serve-{}-{label}",
            std::process::id()
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        std::fs::remove_file(&path)?;
        // SAFETY: plain descriptor duplication on this process's own fds.
        let saved = unsafe { dup(target) };
        if saved < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { dup2(file.as_raw_fd(), target) } < 0 {
            let error = std::io::Error::last_os_error();
            unsafe { close(saved) };
            return Err(error);
        }
        Ok(Self {
            target,
            saved,
            file,
        })
    }

    fn finish(mut self) -> String {
        // SAFETY: restores the descriptor `start` duplicated.
        let restored = unsafe { dup2(self.saved, self.target) };
        unsafe { close(self.saved) };
        if restored < 0 {
            // Every later request would write into an unlinked file. The parent
            // sees the server vanish and runs the compile as a process.
            std::process::exit(70);
        }
        let mut text = Vec::new();
        if self.file.seek(SeekFrom::Start(0)).is_ok() {
            let _ = self.file.read_to_end(&mut text);
        }
        String::from_utf8_lossy(&text).into_owned()
    }
}

/// The protocol of the directory an `Armed` lld is described by, written to `<dir>/version`.
/// `loom-link` links directly when it reads anything else.
const LINK_PROTOCOL: &str = "1";
const LINK_PREFIX: &str = "hash-rustc-link-";
const SIGKILL: i32 = 9;
/// How long `release` waits for a killed lld's exit to be recorded.
const RELEASE_PATIENCE: std::time::Duration = std::time::Duration::from_millis(250);

/// Where link directories live. Fixed when the server starts: a request replaces the process
/// environment, and a different `TMPDIR` per request would hide directories from the startup sweep.
fn link_root() -> &'static std::path::Path {
    static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(std::env::temp_dir)
}

/// What the server records of an lld that has exited: its exit code and, when it did not exit
/// on its own, a line for `stderr` (a killed lld otherwise looks like a silent failure).
fn exit_record(status: std::io::Result<std::process::ExitStatus>) -> (i32, Option<String>) {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Ok(status) => match (status.code(), status.signal()) {
            (Some(code), _) => (code, None),
            (None, Some(signal)) => (1, Some(format!("rust-lld terminated by signal {signal}\n"))),
            (None, None) => (1, Some("rust-lld ended without an exit code\n".to_owned())),
        },
        Err(error) => (1, Some(format!("cannot wait for rust-lld: {error}\n"))),
    }
}

/// An lld started at the beginning of a request, so its startup (about 20 ms, mostly mapping
/// `libLLVM.dylib`) overlaps the compile instead of following it. Its response file is a fifo, so it
/// starts and then waits for its arguments; `loom-link`, which rustc runs as its linker
/// (`-C linker=loom-link`, set by loom-build), writes the real arguments there and reads the result
/// from `status`, `stdout` and `stderr` in `directory`. A request that never links (a compile error)
/// kills the waiting lld. Started only when the request's environment names an lld in
/// `LOOM_LINK_ARM`; anything that goes wrong here means no arming, and `loom-link` then runs the
/// real linker itself.
///
/// The directory is `hash-rustc-link-<server pid>-<n>` and holds `version`, `pid` (the lld),
/// `args` (the fifo), and what `loom-link` reads back. It is removed by `finish`, and by `Drop` on
/// an early return or panic. A server that dies with a request in flight (SIGKILL, a crash, being
/// discarded after a timeout) leaves it and a blocked lld behind: the next server's `sweep_orphans`
/// reaps both.
struct Armed {
    directory: std::path::PathBuf,
    pid: i32,
    waiter: Option<std::thread::JoinHandle<()>>,
    released: bool,
}

impl Armed {
    fn start(lld: &str, request_env: &std::collections::BTreeMap<String, String>) -> Option<Self> {
        use std::os::unix::{ffi::OsStrExt, fs::DirBuilderExt};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let directory = link_root().join(format!(
            "{LINK_PREFIX}{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&directory).ok()?;
        let launch = |directory: &std::path::Path| -> Option<(i32, std::thread::JoinHandle<()>)> {
            std::fs::write(directory.join("version"), LINK_PROTOCOL).ok()?;
            let pipe = directory.join("args");
            let c_pipe = std::ffi::CString::new(pipe.as_os_str().as_bytes()).ok()?;
            // SAFETY: a valid NUL-terminated path.
            if unsafe { mkfifo(c_pipe.as_ptr(), 0o600) } != 0 {
                return None;
            }
            let mut command = std::process::Command::new(lld);
            command
                .args(["-flavor", "wasm"])
                .arg(format!("@{}", pipe.display()))
                .env_clear();
            // What the dynamic loader needs to start this lld, as the same request would give it
            // without the front (an lld that finds its libraries by rpath needs neither).
            for name in ["DYLD_LIBRARY_PATH", "LD_LIBRARY_PATH"] {
                if let Some(value) = request_env.get(name) {
                    command.env(name, value);
                }
            }
            let mut child = command
                .stdin(std::process::Stdio::null())
                .stdout(std::fs::File::create(directory.join("stdout")).ok()?)
                .stderr(std::fs::File::create(directory.join("stderr")).ok()?)
                .spawn()
                .ok()?;
            let pid = i32::try_from(child.id()).ok()?;
            if std::fs::write(directory.join("pid"), pid.to_string()).is_err() {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            let status = directory.join("status");
            let stderr = directory.join("stderr");
            Some((
                pid,
                std::thread::spawn(move || {
                    let (code, note) = exit_record(child.wait());
                    if let Some(note) = note
                        && let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&stderr)
                    {
                        let _ = file.write_all(note.as_bytes());
                    }
                    let temporary = status.with_extension("tmp");
                    if std::fs::write(&temporary, format!("{code}\n")).is_ok() {
                        let _ = std::fs::rename(&temporary, &status);
                    }
                }),
            ))
        };
        match launch(&directory) {
            Some((pid, waiter)) => Some(Self {
                directory,
                pid,
                waiter: Some(waiter),
                released: false,
            }),
            None => {
                let _ = std::fs::remove_dir_all(&directory);
                None
            }
        }
    }

    /// Release the lld and remove the directory (also what `Drop` does).
    fn finish(mut self) {
        self.release();
    }

    /// An lld whose exit is not recorded yet has not been used, or not finished: it is still
    /// blocked on the pipe or still starting. It is killed at once, where coaxing it out with an
    /// empty response file waited for it to start (up to 250 ms on a compile that never links).
    /// The pid is this lld's: the waiter thread reaps the process and only then writes `status`,
    /// so while `status` is absent the process is unreaped. The one gap is the instant between the
    /// reap and the file, where a pid could have been recycled only by a full wrap of the pid space.
    fn release(&mut self) {
        if std::mem::replace(&mut self.released, true) {
            return;
        }
        let status = self.directory.join("status");
        if !status.exists() {
            // SAFETY: a signal to the child this struct started and has not seen exit.
            unsafe { kill(self.pid, SIGKILL) };
            let started = std::time::Instant::now();
            while !status.exists() && started.elapsed() < RELEASE_PATIENCE {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        if status.exists()
            && let Some(waiter) = self.waiter.take()
        {
            let _ = waiter.join();
        }
        // A waiter still blocked is detached and ends with its lld; the directory goes either way.
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        self.release();
    }
}

/// Whether process `pid` exists (kill with signal 0). Unusable ids count as existing, so they are
/// never acted on.
fn process_exists(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid).ok().filter(|pid| *pid > 1) else {
        return true;
    };
    // SAFETY: signal 0 only checks that the process can be signalled.
    unsafe { kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(1) }
}

/// Whether `pid` is still the `rust-lld` that waits on `directory`'s pipe: its command line, as
/// `ps` prints it, names both. A recycled pid fails this and is left alone.
fn is_waiting_lld(pid: i32, directory: &std::path::Path) -> bool {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-o", "command=", "-p"])
        .arg(pid.to_string())
        .output()
    else {
        return false;
    };
    let command = String::from_utf8_lossy(&output.stdout);
    output.status.success()
        && command.contains("rust-lld")
        && command.contains(directory.to_string_lossy().as_ref())
}

/// Reap what servers that died in the middle of a request left in `root`: for every
/// `hash-rustc-link-<pid>-<n>` directory of ours whose server `<pid>` is gone (or is this process,
/// which has created nothing yet, so the directory is an earlier process's with the same pid), kill
/// the lld named in its `pid` file if it is still that lld, then remove the directory.
fn sweep_orphans(root: &std::path::Path) {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let own = std::process::id();
    // SAFETY: no preconditions.
    let euid = unsafe { geteuid() };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(owner) = file_name
            .to_str()
            .and_then(|name| name.strip_prefix(LINK_PREFIX))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if owner != own && process_exists(owner) {
            continue;
        }
        // `DirEntry::metadata` does not follow links; another user's directory is not ours to touch.
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() || metadata.uid() != euid {
            continue;
        }
        let directory = entry.path();
        let pid = std::fs::read_to_string(directory.join("pid"))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
            .filter(|pid| *pid > 1);
        if let Some(pid) = pid
            && is_waiting_lld(pid, &directory)
        {
            // SAFETY: the process was just verified to be that lld.
            unsafe { kill(pid, SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&directory);
    }
}

fn handle(request: Request) -> Reply {
    if let Some(name) = request
        .env
        .iter()
        .find(|(name, value)| {
            name.is_empty() || name.contains(['=', '\0']) || value.contains('\0')
        })
        .map(|(name, _)| name)
    {
        return Reply {
            success: false,
            stdout: String::new(),
            stderr: format!("hash-rustc --loom-serve: invalid environment entry {name:?}\n"),
        };
    }
    // SAFETY: requests are served one at a time and no compiler thread is alive
    // between them; see the module comment.
    unsafe {
        for (name, _) in std::env::vars_os().collect::<Vec<_>>() {
            std::env::remove_var(name);
        }
        for (name, value) in &request.env {
            std::env::set_var(name, value);
        }
    }
    let failed = |error: std::io::Error| Reply {
        success: false,
        stdout: String::new(),
        stderr: format!("hash-rustc --loom-serve: {error}\n"),
    };
    if let Err(error) = std::env::set_current_dir(&request.cwd) {
        return failed(error);
    }
    let out = match Capture::start(1, "out") {
        Ok(capture) => capture,
        Err(error) => return failed(error),
    };
    let err = match Capture::start(2, "err") {
        Ok(capture) => capture,
        Err(error) => {
            out.finish();
            return failed(error);
        }
    };
    let armed = request
        .env
        .get("LOOM_LINK_ARM")
        .and_then(|lld| Armed::start(lld, &request.env));
    if let Some(armed) = &armed {
        // SAFETY: as above, requests are served one at a time.
        unsafe { std::env::set_var("LOOM_LINK_DIR", &armed.directory) };
    }
    let code = crate::compile(request.args);
    if let Some(armed) = armed {
        armed.finish();
    }
    let stderr = err.finish();
    let stdout = out.finish();
    Reply {
        success: code == ExitCode::SUCCESS,
        stdout,
        stderr,
    }
}

pub fn serve() -> ExitCode {
    crate::timing_enabled();
    // Before the first request replaces the environment, and before `ready`: a parent that sees
    // `ready` sees the directories of dead servers already gone.
    sweep_orphans(link_root());
    // SAFETY: the parent passed one end of a socket pair as our standard input;
    // this takes ownership of that descriptor, and nothing else reads stdin.
    let stream = unsafe { UnixStream::from_raw_fd(0) };
    let mut writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(error) => {
            eprintln!("hash-rustc --loom-serve: cannot duplicate the request socket: {error}");
            return ExitCode::FAILURE;
        }
    };
    if writer.write_all(b"ready\n").is_err() {
        return ExitCode::FAILURE;
    }
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            // The parent closed its end: it is gone.
            Ok(0) | Err(_) => return ExitCode::SUCCESS,
            Ok(_) => {}
        }
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(request) => handle(request),
            Err(error) => Reply {
                success: false,
                stdout: String::new(),
                stderr: format!("hash-rustc --loom-serve: bad request: {error}\n"),
            },
        };
        if serde_json::to_writer(&mut writer, &reply).is_err() || writer.write_all(b"\n").is_err()
        {
            return ExitCode::SUCCESS;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn an_lld_that_was_killed_is_recorded_with_its_signal_not_as_a_silent_failure() {
        assert_eq!(
            exit_record(Ok(std::process::ExitStatus::from_raw(3 << 8))),
            (3, None)
        );
        let (code, note) = exit_record(Ok(std::process::ExitStatus::from_raw(9)));
        assert_eq!(code, 1);
        assert_eq!(note.as_deref(), Some("rust-lld terminated by signal 9\n"));
    }
}
