use crate::{Store, sharedcore};
use std::os::unix::thread::JoinHandleExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Name of the child half below, as libtest spells it.
const PROBE: &str = "wasm_engine::tests::signalling_the_engines_threads_does_not_abort";

/// A signal that reaches Wasmtime's Mach handler thread aborts the process, so
/// the probe runs as a child: here a dead process is a failed assertion instead
/// of a test binary that vanishes mid-run, which is exactly how this bug used to
/// present (`cargo test -p loom-rt --lib`, signal 6, no surviving diagnostic).
#[test]
fn mach_exception_handler_thread_cannot_be_signalled() {
    let output = std::process::Command::new(std::env::current_exe().expect("test binary path"))
        .args(["--exact", "--ignored", "--nocapture", PROBE])
        .output()
        .expect("run the probe");
    assert!(
        output.status.success(),
        "{PROBE} exited {:?}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

static DELIVERED: AtomicUsize = AtomicUsize::new(0);

extern "C" fn count_delivery(_signal: libc::c_int) {
    DELIVERED.fetch_add(1, Ordering::Relaxed);
}

/// This task's own port, the handle `task_threads` reports threads against.
fn task() -> libc::mach_port_t {
    unsafe { mach2::traps::mach_task_self() }
}

/// Mach port names of every thread in this process, which is how a thread this
/// process never created is found.
fn thread_ports() -> Vec<libc::mach_port_t> {
    unsafe {
        let mut list: libc::thread_act_array_t = std::ptr::null_mut();
        let mut count: libc::mach_msg_type_number_t = 0;
        assert_eq!(
            libc::task_threads(task(), &mut list, &mut count),
            0,
            "task_threads"
        );
        let ports = std::slice::from_raw_parts(list, count as usize).to_vec();
        libc::vm_deallocate(
            task(),
            list as libc::vm_address_t,
            (count as usize * size_of::<libc::mach_port_t>()) as libc::vm_size_t,
        );
        ports
    }
}

fn signal(thread: libc::pthread_t) {
    assert_eq!(
        unsafe { libc::pthread_kill(thread, libc::SIGCHLD) },
        0,
        "pthread_kill"
    );
}

fn wait_for_delivery(count: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if DELIVERED.load(Ordering::Relaxed) >= count {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

/// Hands Wasmtime's own threads the signal Tokio's child reaping puts in the air.
/// Before the mask in `create`, the handler thread woke from `mach_msg` with
/// `MACH_RCV_INTERRUPTED` and aborted the process here.
///
/// The control comes first: an ordinary thread of this process is signalled the
/// same way and the handler must count it, so that surviving the second half
/// means the signals were blocked rather than never sent.
#[test]
#[ignore = "child half of mach_exception_handler_thread_cannot_be_signalled"]
fn signalling_the_engines_threads_does_not_abort() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = count_delivery as extern "C" fn(libc::c_int) as usize;
        action.sa_flags = libc::SA_RESTART;
        assert_eq!(libc::sigemptyset(&mut action.sa_mask), 0);
        assert_eq!(
            libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()),
            0,
            "install the SIGCHLD handler Tokio installs to reap children"
        );
    }

    let before = thread_ports();
    let (engine, _cache) =
        sharedcore::engine(Store::memory().expect("store")).expect("engine and its handler thread");
    let spawned: Vec<_> = thread_ports()
        .into_iter()
        .filter(|port| !before.contains(port))
        .collect();
    assert!(
        !spawned.is_empty(),
        "the engine spawned no thread, so this probe measures nothing"
    );

    let control = std::thread::spawn(|| while !wait_for_delivery(1) {});
    signal(control.as_pthread_t());
    assert!(
        wait_for_delivery(1),
        "a signal aimed at an ordinary thread was never delivered"
    );
    control.join().expect("control thread");

    for port in spawned {
        let thread = unsafe { libc::pthread_from_mach_thread_np(port) };
        assert_ne!(thread, 0, "mach port {port} is not a pthread");
        for _ in 0..16 {
            signal(thread);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    std::thread::sleep(Duration::from_millis(50));
    drop(engine);
}
