//! `-C linker=loom-link`: the linker front for the served compiler.
//!
//! `rust-lld` costs about 20 ms to start (it maps a 137 MB `libLLVM.dylib` before it reads its
//! arguments) and about 4 ms to link a guest. The compiler server (`tools/hash-rustc`, `serve.rs`)
//! starts an lld at the beginning of a compile request with `@<dir>/args`, a named pipe as its
//! response file, so the 20 ms overlaps the compile. When rustc reaches the link it runs this
//! program with the real arguments. With `LOOM_LINK_DIR` naming such a directory, this writes the
//! arguments into the pipe, waits for the server to record lld's exit (`status`), replays lld's
//! output and exits with its code. Without it, or whenever the waiting lld cannot be used, it runs
//! `rust-lld` directly, exactly as rustc would have.
//!
//! Directory layout, owned by the server: `version` (the protocol this program speaks, `1`; any
//! other content means the server is not one this program understands), `pid` (the waiting lld),
//! `args` (fifo, removed here once used), `status` (lld's exit code, written by the server after
//! lld exits), `stdout` and `stderr` (lld's output, `stderr` ending in a line naming the signal
//! when lld was killed).
//!
//! The direct path is taken when nothing has been handed to the waiting lld: no directory, no
//! pipe, another protocol version, an empty argument (see [`quote`]), lld dead before it read its
//! arguments, or a pipe that broke while writing them. Once lld has been handed the arguments there
//! is no retry, and a vanished server or directory is an error, not a wait.
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
/// The protocol the server writes into `version`.
const PROTOCOL: &str = "1";

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
        Some(directory) if armed_for(&directory, &arguments) => armed(&directory, &arguments),
        _ => direct(&arguments),
    }
}

/// Whether the waiting lld in `directory` can be handed `arguments`.
fn armed_for(directory: &Path, arguments: &[OsString]) -> bool {
    directory.join("args").exists()
        && fs::read_to_string(directory.join("version")).is_ok_and(|text| text.trim() == PROTOCOL)
        // LLVM's response-file tokenizer (`cl::TokenizeGNUCommandLine`) pushes a token only when
        // it is non-empty, so `""` in the file would vanish instead of reaching lld as an empty
        // argument. The command line keeps it: link directly.
        && !arguments.iter().any(|argument| argument.is_empty())
}

/// `rust-lld -flavor wasm <arguments>`, replacing this process.
fn direct(arguments: &[OsString]) -> ExitCode {
    let lld = std::env::var_os("LOOM_RUST_LLD").unwrap_or_else(|| "rust-lld".into());
    let error = Command::new(&lld)
        .arg("-flavor")
        .arg("wasm")
        .args(arguments)
        .exec();
    eprintln!(
        "loom-link: cannot run {}: {error}",
        Path::new(&lld).display()
    );
    ExitCode::from(127)
}

/// One argument in the posix quoting LLVM's response files read: double quotes, backslash escapes.
/// An empty argument quotes to `""`, which LLVM's tokenizer drops: `armed_for` never lets one in.
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

/// Why waiting stopped without lld's answer.
enum Lost {
    /// The process that ran this one (the compiler server) is gone; nobody wants the link.
    Parent,
    /// The server removed its directory: it gave up on this request.
    Directory,
    Patience,
}

/// The conditions under which waiting on the server is pointless.
struct Watch<'a> {
    directory: &'a Path,
    parent: libc::pid_t,
    started: Instant,
}

impl<'a> Watch<'a> {
    fn new(directory: &'a Path) -> Self {
        Self {
            directory,
            // SAFETY: getppid has no preconditions.
            parent: unsafe { libc::getppid() },
            started: Instant::now(),
        }
    }

    /// An orphaned process is reparented (to pid 1 or a subreaper), so a changed parent is gone.
    fn lost(&self) -> Option<Lost> {
        // SAFETY: getppid has no preconditions.
        if unsafe { libc::getppid() } != self.parent {
            Some(Lost::Parent)
        } else if !self.directory.exists() {
            Some(Lost::Directory)
        } else if self.started.elapsed() > PATIENCE {
            Some(Lost::Patience)
        } else {
            None
        }
    }

    /// Wait for the server to record lld's exit.
    fn status(&self) -> Result<(), Lost> {
        let status = self.directory.join("status");
        while !status.exists() {
            if let Some(lost) = self.lost() {
                return Err(lost);
            }
            thread::sleep(POLL);
        }
        Ok(())
    }
}

fn gone(lost: Lost) -> ExitCode {
    eprintln!(
        "loom-link: {}",
        match lost {
            Lost::Parent => "the compiler server that started this link is gone",
            Lost::Directory => "the server removed the linker directory before the linker finished",
            Lost::Patience => "the linker did not finish",
        }
    );
    ExitCode::from(1)
}

fn armed(directory: &Path, arguments: &[OsString]) -> ExitCode {
    let watch = Watch::new(directory);
    let pipe = directory.join("args");
    let status = directory.join("status");
    // Opening a fifo for writing succeeds once lld has it open for reading. Without a reader yet,
    // a non-blocking open fails with ENXIO: wait for one, unless lld has already died. Nothing has
    // been handed over while this loops, so every way out but a dead parent links directly.
    let mut writer = loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&pipe)
        {
            Ok(file) => break file,
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {}
            // The pipe is gone (used, or the directory was removed).
            Err(_) => return direct(arguments),
        }
        // lld exited without ever reading its arguments (a flag it does not know, a missing library).
        if status.exists() {
            return direct(arguments);
        }
        match watch.lost() {
            Some(Lost::Parent) => return gone(Lost::Parent),
            Some(_) => return direct(arguments),
            None => {}
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
    let written = writer.write_all(&text);
    drop(writer);
    if written.is_err() {
        // lld closed its end before it had read everything: it died. Once its status says so no
        // second linker can collide with it on the output file, and the arguments link directly.
        return match watch.status() {
            Ok(()) => {
                let _ = fs::remove_file(&pipe);
                direct(arguments)
            }
            Err(lost) => gone(lost),
        };
    }
    match watch.status() {
        Ok(()) => finish(directory, &pipe),
        Err(lost) => {
            let _ = fs::remove_file(&pipe);
            gone(lost)
        }
    }
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
        assert_eq!(
            strip(&["-flavor", "wasm", "--no-entry", "a.o"]),
            ["--no-entry", "a.o"]
        );
        assert_eq!(
            strip(&["--no-entry", "-flavor", "wasm"]),
            ["--no-entry", "-flavor", "wasm"]
        );
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

    #[test]
    fn an_empty_argument_or_another_protocol_is_never_handed_to_the_waiting_linker() {
        let directory =
            std::env::temp_dir().join(format!("loom-link-armed-for-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("args"), b"").unwrap();
        let arguments = |items: &[&str]| items.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(
            !armed_for(&directory, &arguments(&["a"])),
            "no version file"
        );
        fs::write(directory.join("version"), "1\n").unwrap();
        assert!(armed_for(&directory, &arguments(&["a", "b"])));
        assert!(!armed_for(&directory, &arguments(&["a", "", "b"])));
        fs::write(directory.join("version"), "2\n").unwrap();
        assert!(!armed_for(&directory, &arguments(&["a"])));
        fs::remove_dir_all(&directory).unwrap();
    }
}
