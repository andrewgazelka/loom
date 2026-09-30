//! Compiles served by long-lived `hash-rustc --loom-serve` processes.
//!
//! A process per compile pays about 45 ms to load `librustc_driver` before it
//! reads source; a served compile of a small guest measures about 28 ms in all.
//! The pool holds idle servers: a compile takes one (starting a new one when
//! none is idle), and returns it afterwards, so builds in different cache
//! directories still run side by side. A server that fails mid-request is
//! discarded and the caller falls back to an ordinary process, which reproduces
//! any real failure with the compiler's own exit status.
//!
//! The transport is a socket pair whose far end is the server's standard input:
//! no path on the filesystem for another user to connect to, no length limit,
//! no stale file, and the server exits when this end closes.
use super::*;
use std::process::Stdio;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::Child,
};

/// A served compile stops being reused after this many requests, bounding
/// whatever a long-lived compiler process accumulates.
const REQUESTS_PER_SERVER: u32 = 2000;
const IDLE_SERVERS: usize = 4;
/// Consecutive failures to start a server before compiles stop trying and use
/// processes directly (a driver without `--loom-serve`, a broken install).
const START_FAILURES_BEFORE_GIVING_UP: u32 = 3;

#[derive(Default)]
pub struct RustcServers {
    idle: std::sync::Mutex<Vec<Server>>,
    start_failures: std::sync::atomic::AtomicU32,
}

struct Server {
    driver: PathBuf,
    requests: u32,
    stream: BufReader<UnixStream>,
    // Killed on drop; it also exits by itself when `stream` closes.
    _child: Child,
}

#[derive(serde::Serialize)]
struct Request<'a> {
    cwd: &'a Path,
    env: BTreeMap<String, String>,
    args: Vec<String>,
}

#[derive(serde::Deserialize)]
struct Reply {
    success: bool,
    stdout: String,
    stderr: String,
}

/// The process description a served compile needs, or `None` when `command`
/// is not a plain invocation of `driver` (sandboxed builds run a wrapper).
///
/// A served request carries exactly the environment given here. The process
/// form is the same only when the command cleared its inherited environment
/// first, which `compiler_environment` does for every non-sandboxed caller;
/// `Command` cannot report that, so a new caller must clear it too.
fn describe<'a>(command: &'a Command, driver: &Path) -> Option<Request<'a>> {
    let std = command.as_std();
    if Path::new(std.get_program()) != driver {
        return None;
    }
    let cwd = std.get_current_dir()?;
    let mut env = BTreeMap::new();
    for (name, value) in std.get_envs() {
        if let Some(value) = value {
            env.insert(name.to_str()?.to_owned(), value.to_str()?.to_owned());
        }
    }
    let mut args = vec![driver.to_str()?.to_owned()];
    for argument in std.get_args() {
        args.push(argument.to_str()?.to_owned());
    }
    Some(Request { cwd, env, args })
}

impl RustcServers {
    /// Run `command` (an invocation of `driver`) on a served compiler, or as a
    /// process when it cannot be served or no server can be reached.
    pub(super) async fn run(
        &self,
        command: Command,
        driver: &Path,
        deadline: std::time::Duration,
    ) -> Result<std::process::Output, BuildError> {
        if let Some(request) = describe(&command, driver)
            && let Some(output) = self.serve(driver, &request, deadline).await?
        {
            return Ok(output);
        }
        super::run(command).await
    }

    /// `Ok(None)`: the server could not be used; the caller runs a process.
    async fn serve(
        &self,
        driver: &Path,
        request: &Request<'_>,
        deadline: std::time::Duration,
    ) -> Result<Option<std::process::Output>, BuildError> {
        let mut server = match self.take(driver) {
            Some(server) => server,
            None => match self.start(driver).await {
                Some(server) => server,
                None => return Ok(None),
            },
        };
        let exchange = tokio::time::timeout(deadline, server.exchange(request)).await;
        let reply = match exchange {
            Ok(Ok(reply)) => reply,
            Ok(Err(error)) => {
                eprintln!("loom-build: compiler server failed ({error}); compiling in a process");
                return Ok(None);
            }
            Err(_) => {
                // `server` is dropped here and killed. It dies without cleaning up, so an lld it
                // had armed (`linker.rs`) and its `hash-rustc-link-<pid>-<n>` directory are left
                // for the next server's startup sweep (`tools/hash-rustc/src/serve.rs`).
                return Err(rejected(format!(
                    "root Rust compiler exceeded {} seconds",
                    deadline.as_secs()
                )));
            }
        };
        server.requests += 1;
        if server.requests < REQUESTS_PER_SERVER {
            let mut idle = self.idle.lock().expect("compiler pool lock poisoned");
            if idle.len() < IDLE_SERVERS {
                idle.push(server);
            }
        }
        Ok(Some(output(reply)))
    }

    fn take(&self, driver: &Path) -> Option<Server> {
        let mut idle = self.idle.lock().expect("compiler pool lock poisoned");
        // A different driver (a rebuilt or re-pinned one) leaves stale servers.
        idle.retain(|server| server.driver == driver);
        idle.pop()
    }

    async fn start(&self, driver: &Path) -> Option<Server> {
        use std::sync::atomic::Ordering;
        if self.start_failures.load(Ordering::Relaxed) >= START_FAILURES_BEFORE_GIVING_UP {
            return None;
        }
        let server = Self::spawn(driver).await;
        match &server {
            Ok(_) => self.start_failures.store(0, Ordering::Relaxed),
            Err(error) => {
                let failures = self.start_failures.fetch_add(1, Ordering::Relaxed) + 1;
                eprintln!(
                    "loom-build: cannot start a compiler server ({error}); compiling in a process{}",
                    if failures >= START_FAILURES_BEFORE_GIVING_UP {
                        " from now on"
                    } else {
                        ""
                    }
                );
            }
        }
        server.ok()
    }

    async fn spawn(driver: &Path) -> std::io::Result<Server> {
        let (parent, child_end) = UnixStream::pair()?;
        let child_end = child_end.into_std()?;
        child_end.set_nonblocking(false)?;
        let mut command = Command::new(driver);
        command
            .arg("--loom-serve")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", std::env::var_os("HOME").unwrap_or_default())
            .envs(std::env::var_os("LOOM_DRIVER_TIMING").map(|value| ("LOOM_DRIVER_TIMING", value)))
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(child_end)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let child = command.spawn()?;
        let mut stream = BufReader::new(parent);
        let mut ready = String::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            stream.read_line(&mut ready),
        )
        .await
        .map_err(|_| std::io::Error::other("no ready line within 20 seconds"))??;
        if read == 0 || ready.trim() != "ready" {
            return Err(std::io::Error::other("the server did not say ready"));
        }
        Ok(Server {
            driver: driver.to_owned(),
            requests: 0,
            stream,
            _child: child,
        })
    }
}

impl Server {
    async fn exchange(&mut self, request: &Request<'_>) -> std::io::Result<Reply> {
        let mut line = serde_json::to_vec(request).map_err(std::io::Error::other)?;
        line.push(b'\n');
        self.stream.get_mut().write_all(&line).await?;
        let mut reply = String::new();
        if self.stream.read_line(&mut reply).await? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        serde_json::from_str(&reply).map_err(std::io::Error::other)
    }
}

fn output(reply: Reply) -> std::process::Output {
    use std::os::unix::process::ExitStatusExt;
    std::process::Output {
        status: std::process::ExitStatus::from_raw(if reply.success { 0 } else { 1 << 8 }),
        stdout: reply.stdout.into_bytes(),
        stderr: reply.stderr.into_bytes(),
    }
}
