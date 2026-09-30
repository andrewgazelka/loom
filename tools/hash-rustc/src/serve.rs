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

/// An lld started at the beginning of a request, so its startup (about 20 ms, mostly mapping
/// `libLLVM.dylib`) overlaps the compile instead of following it. Its response file is a fifo, so it
/// starts and then waits for its arguments; `loom-link`, which rustc runs as its linker
/// (`-C linker=loom-link`, set by loom-build), writes the real arguments there and reads the result
/// from `status`, `stdout` and `stderr` in `directory`. A request that never links (a compile error)
/// releases the waiting lld with an empty response. Started only when the request's environment
/// names an lld in `LOOM_LINK_ARM`; anything that goes wrong here means no arming, and `loom-link`
/// then runs the real linker itself.
/// `O_NONBLOCK`; the two platforms this runs on disagree on its value.
#[cfg(target_os = "macos")]
const O_NONBLOCK: i32 = 0x4;
#[cfg(not(target_os = "macos"))]
const O_NONBLOCK: i32 = 0x800;

struct Armed {
    directory: std::path::PathBuf,
    waiter: Option<std::thread::JoinHandle<()>>,
}

impl Armed {
    fn start(lld: &str) -> Option<Self> {
        use std::os::unix::{ffi::OsStrExt, fs::DirBuilderExt};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "hash-rustc-link-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&directory).ok()?;
        let armed = |directory: &std::path::Path| -> Option<std::thread::JoinHandle<()>> {
            let pipe = directory.join("args");
            let c_pipe = std::ffi::CString::new(pipe.as_os_str().as_bytes()).ok()?;
            // SAFETY: a valid NUL-terminated path.
            if unsafe { mkfifo(c_pipe.as_ptr(), 0o600) } != 0 {
                return None;
            }
            let mut child = std::process::Command::new(lld)
                .args(["-flavor", "wasm"])
                .arg(format!("@{}", pipe.display()))
                .env_clear()
                .stdin(std::process::Stdio::null())
                .stdout(std::fs::File::create(directory.join("stdout")).ok()?)
                .stderr(std::fs::File::create(directory.join("stderr")).ok()?)
                .spawn()
                .ok()?;
            let status = directory.join("status");
            Some(std::thread::spawn(move || {
                let code = child.wait().ok().and_then(|status| status.code()).unwrap_or(-1);
                let temporary = status.with_extension("tmp");
                if std::fs::write(&temporary, format!("{code}\n")).is_ok() {
                    let _ = std::fs::rename(&temporary, &status);
                }
            }))
        };
        match armed(&directory) {
            Some(waiter) => Some(Self {
                directory,
                waiter: Some(waiter),
            }),
            None => {
                let _ = std::fs::remove_dir_all(&directory);
                None
            }
        }
    }

    /// Release an lld that was never used and remove the directory.
    fn finish(mut self) {
        use std::os::unix::fs::OpenOptionsExt;
        let status = self.directory.join("status");
        let pipe = self.directory.join("args");
        let started = std::time::Instant::now();
        while !status.exists() && started.elapsed() < std::time::Duration::from_millis(250) {
            // Opening the fifo for writing and closing it again is an empty response file: lld
            // reports that it has no input and exits. It fails while no reader has the pipe yet
            // (lld still starting) and once the pipe was used (`loom-link` removes it).
            if pipe.exists() {
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(O_NONBLOCK)
                    .open(&pipe);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        if status.exists() {
            if let Some(waiter) = self.waiter.take() {
                let _ = waiter.join();
            }
        }
        // A waiter still blocked is left to end with its lld; the directory goes either way.
        let _ = std::fs::remove_dir_all(&self.directory);
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
        .and_then(|lld| Armed::start(lld));
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
