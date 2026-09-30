//! `hash-rustc --loom-serve <socket>`: one long-lived compiler process.
//!
//! Starting rustc costs about 45 ms (loading `librustc_driver`) before it reads
//! a byte of source, and a small guest compiles in about 35 ms once it is
//! running, so a process per compile is most of a cell's latency. This mode
//! accepts one request at a time on a Unix socket and runs the same driver
//! entry (`crate::compile`) in this process: `rustc_driver::run_compiler` is
//! re-entrant (100 back-to-back compiles measured flat at 36 ms with a stable
//! resident set).
//!
//! A request is one line of JSON, `{"cwd", "env", "args"}`: the working
//! directory, the complete environment and the compiler arguments a process
//! would have been given. The reply is one line, `{"success", "stdout",
//! "stderr"}`. The process environment and the standard streams are per-request
//! global state, which is sound only because requests are served one at a time
//! and no compiler thread outlives its request.
//!
//! The server exits when its standard input closes, so it never outlives the
//! process that started it.
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::process::ExitCode;

unsafe extern "C" {
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
        unsafe {
            dup2(self.saved, self.target);
            close(self.saved);
        }
        let mut text = Vec::new();
        if self.file.seek(SeekFrom::Start(0)).is_ok() {
            let _ = self.file.read_to_end(&mut text);
        }
        String::from_utf8_lossy(&text).into_owned()
    }
}

fn handle(request: Request) -> Reply {
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
    let code = crate::compile(request.args);
    let stderr = err.finish();
    let stdout = out.finish();
    Reply {
        success: code == ExitCode::SUCCESS,
        stdout,
        stderr,
    }
}

pub fn serve(socket: &str) -> ExitCode {
    // The parent holds our stdin open; its exit closes it.
    std::thread::spawn(|| {
        let mut sink = [0u8; 64];
        let mut stdin = std::io::stdin();
        while matches!(stdin.read(&mut sink), Ok(read) if read > 0) {}
        std::process::exit(0);
    });
    let _ = std::fs::remove_file(socket);
    let listener = match UnixListener::bind(socket) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("hash-rustc --loom-serve: cannot bind {socket}: {error}");
            return ExitCode::FAILURE;
        }
    };
    println!("ready");
    let _ = std::io::stdout().flush();
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
            continue;
        }
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(request) => handle(request),
            Err(error) => Reply {
                success: false,
                stdout: String::new(),
                stderr: format!("hash-rustc --loom-serve: bad request: {error}\n"),
            },
        };
        let mut stream = stream;
        let _ = serde_json::to_writer(&mut stream, &reply);
        let _ = stream.write_all(b"\n");
    }
    ExitCode::SUCCESS
}
