//! Every Wasmtime engine in this crate is built here, so that the handler thread
//! Wasmtime spawns behind the first one cannot be handed a signal.
//!
//! On macOS Wasmtime catches guest faults through Mach exceptions rather than
//! signals: the first `Engine` in a process spawns a handler thread that parks in
//! `mach_msg` with `MACH_RCV_INTERRUPT` set. A signal delivered to that thread
//! makes the trap return `MACH_RCV_INTERRUPTED`, and wasmtime-48.0.1
//! (`runtime/vm/sys/unix/machports.rs:228`) answers any receive failure other than
//! a destroyed port with `eprintln!` plus `libc::abort()`, which kills the whole
//! process. A process-directed signal lands on an arbitrary thread that does not
//! block it, and this process gains a `SIGCHLD` handler the moment Tokio reaps its
//! first child, so the handler thread has to be born unable to receive one: a new
//! thread inherits the signal mask of the thread that creates it.
use anyhow::Result;
use wasmtime::{Config, Engine};

/// The hazard and the instrument that measures it are both Mach, so the test
/// lives on the platform that has them.
#[cfg(all(test, target_os = "macos"))]
mod tests;

/// Builds an engine with asynchronous signals blocked, so that any thread
/// Wasmtime spawns inherits a mask that excludes it from process-directed
/// delivery. Engines after the first pay a mask swap and nothing else.
pub fn create(config: &Config) -> Result<Engine> {
    let _blocked = AsyncSignals::blocked();
    Engine::new(config).map_err(|error| anyhow::anyhow!("{error:#}"))
}

/// Signals a thread raises against itself. Blocking these turns a fault into an
/// immediate kill instead of a handled trap, so they stay deliverable; every
/// other signal belongs to whichever thread the process nominates, and the
/// threads created under this guard decline the nomination.
const SYNCHRONOUS: [libc::c_int; 6] = [
    libc::SIGSEGV,
    libc::SIGBUS,
    libc::SIGILL,
    libc::SIGFPE,
    libc::SIGTRAP,
    libc::SIGSYS,
];

/// The calling thread's mask, restored on drop including on the way out of a
/// panic.
struct AsyncSignals(libc::sigset_t);

impl AsyncSignals {
    fn blocked() -> Self {
        unsafe {
            let mut blocked: libc::sigset_t = std::mem::zeroed();
            let mut previous: libc::sigset_t = std::mem::zeroed();
            check(libc::sigfillset(&mut blocked), "sigfillset");
            for signal in SYNCHRONOUS {
                check(libc::sigdelset(&mut blocked, signal), "sigdelset");
            }
            check(
                libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous),
                "pthread_sigmask",
            );
            Self(previous)
        }
    }
}

impl Drop for AsyncSignals {
    fn drop(&mut self) {
        unsafe {
            check(
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut()),
                "pthread_sigmask",
            );
        }
    }
}

/// These calls fail only on arguments this module does not build, and a silent
/// failure would leave the handler thread signallable again.
fn check(code: libc::c_int, call: &str) {
    assert_eq!(code, 0, "{call}: {}", std::io::Error::last_os_error());
}
