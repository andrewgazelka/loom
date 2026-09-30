//! `-C linker=loom-link`: the linker front for the served compiler.
//!
//! `rust-lld` costs about 20 ms to start (it maps a 137 MB `libLLVM.dylib` before it reads its
//! arguments) and about 4 ms to link a guest. The compiler server (`tools/hash-rustc`, `serve.rs`)
//! starts an lld at the beginning of a compile request with `@<dir>/args`, a named pipe as its
//! response file, so the 20 ms overlaps the compile. When rustc reaches the link it runs this
//! program with the real arguments. With `LOOM_LINK_DIR` naming such a directory, this writes the
//! arguments into the pipe, waits for the server to record lld's exit (`status`), replays lld's
//! output and exits with its code. Without it, or once the pipe is used up, it runs `rust-lld`
//! directly, exactly as rustc would have.
//!
//! Directory layout, owned by the server: `args` (fifo, removed here once used), `status` (lld's
//! exit code, written by the server after lld exits), `stdout` and `stderr` (lld's output).
use std::{
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    thread,
    time::{Duration, Instant},
};

/// Longest a link may take before this gives up, as the server's own deadline is longer.
const PATIENCE: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_micros(250);

/// rustc runs its linker as `<linker> -flavor wasm <arguments>` even when told the flavor, and
/// lld takes `-flavor` only as its first argument: the running lld already has it, and the direct
/// path adds its own.
fn without_flavor(mut arguments: Vec<OsString>) -> Vec<OsString> {
    if arguments.len() >= 2 && arguments[0] == "-flavor" {
        arguments.drain(..2);
    }
    arguments
}

fn main() -> ExitCode {
    let arguments = without_flavor(std::env::args_os().skip(1).collect());
    match std::env::var_os("LOOM_LINK_DIR").map(PathBuf::from) {
        Some(directory) if directory.join("args").exists() => armed(&directory, &arguments),
        _ => direct(&arguments),
    }
}

/// `rust-lld -flavor wasm <arguments>`, replacing this process.
fn direct(arguments: &[OsString]) -> ExitCode {
    let lld = std::env::var_os("LOOM_RUST_LLD").unwrap_or_else(|| "rust-lld".into());
    let error = Command::new(&lld)
        .arg("-flavor")
        .arg("wasm")
        .args(arguments)
        .exec();
    eprintln!("loom-link: cannot run {}: {error}", Path::new(&lld).display());
    ExitCode::from(127)
}

/// One argument in the posix quoting LLVM's response files read: double quotes, backslash escapes.
pub fn quote(argument: &OsStr) -> Vec<u8> {
    let mut out = vec![b'"'];
    for &byte in argument.as_bytes() {
        if byte == b'\\' || byte == b'"' {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out.push(b'"');
    out
}

fn armed(directory: &Path, arguments: &[OsString]) -> ExitCode {
    let started = Instant::now();
    let pipe = directory.join("args");
    let status = directory.join("status");
    // Opening a fifo for writing succeeds once lld has it open for reading. Without a reader yet,
    // a non-blocking open fails with ENXIO: wait for one, unless lld has already died.
    let mut writer = loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&pipe)
        {
            Ok(file) => break file,
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {}
            Err(error) => {
                eprintln!("loom-link: cannot open {}: {error}", pipe.display());
                return ExitCode::from(1);
            }
        }
        if status.exists() {
            return finish(directory, &pipe);
        }
        if started.elapsed() > PATIENCE {
            eprintln!("loom-link: the waiting linker never read its arguments");
            let _ = fs::remove_file(&pipe);
            return ExitCode::from(1);
        }
        thread::sleep(POLL);
    };
    // Back to blocking writes: a long argument list may need lld to read while this writes.
    unsafe {
        let fd = std::os::fd::AsRawFd::as_raw_fd(&writer);
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
    }
    let mut text = Vec::new();
    for argument in arguments {
        text.extend(quote(argument));
        text.push(b'\n');
    }
    if let Err(error) = writer.write_all(&text) {
        eprintln!("loom-link: cannot hand the arguments to the waiting linker: {error}");
        let _ = fs::remove_file(&pipe);
        return ExitCode::from(1);
    }
    drop(writer);
    while !status.exists() {
        if started.elapsed() > PATIENCE {
            eprintln!("loom-link: the linker did not finish");
            let _ = fs::remove_file(&pipe);
            return ExitCode::from(1);
        }
        thread::sleep(POLL);
    }
    finish(directory, &pipe)
}

/// Replay lld's output and return its exit code; the pipe is used up either way.
fn finish(directory: &Path, pipe: &Path) -> ExitCode {
    let _ = fs::remove_file(pipe);
    let code: i32 = fs::read_to_string(directory.join("status"))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(1);
    if let Ok(bytes) = fs::read(directory.join("stdout")) {
        let _ = std::io::stdout().write_all(&bytes);
    }
    if let Ok(bytes) = fs::read(directory.join("stderr")) {
        let _ = std::io::stderr().write_all(&bytes);
    }
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leading_flavor_pair_is_dropped_and_nothing_else() {
        let strip = |items: &[&str]| -> Vec<String> {
            without_flavor(items.iter().map(OsString::from).collect())
                .into_iter()
                .map(|item| item.into_string().unwrap())
                .collect()
        };
        assert_eq!(strip(&["-flavor", "wasm", "--no-entry", "a.o"]), ["--no-entry", "a.o"]);
        assert_eq!(strip(&["--no-entry", "-flavor", "wasm"]), ["--no-entry", "-flavor", "wasm"]);
        assert_eq!(strip(&["-flavor"]), ["-flavor"]);
        assert!(strip(&[]).is_empty());
    }

    #[test]
    fn quoting_survives_llvm_response_file_parsing_for_awkward_arguments() {
        assert_eq!(quote(OsStr::new("plain")), b"\"plain\"");
        assert_eq!(quote(OsStr::new("a b")), b"\"a b\"");
        assert_eq!(quote(OsStr::new("say \"hi\"")), b"\"say \\\"hi\\\"\"");
        assert_eq!(quote(OsStr::new("back\\slash")), b"\"back\\\\slash\"");
        assert_eq!(quote(OsStr::new("")), b"\"\"");
    }
}
