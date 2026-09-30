//! Compiles served by long-lived `hash-rustc --loom-serve` processes.
//!
//! A process per compile pays about 45 ms to load `librustc_driver` before it
//! reads source; a served compile of a small guest measures about 28 ms in all.
//! The pool holds idle servers: a compile takes one (starting a new one when
//! none is idle), and returns it afterwards, so builds in different cache
//! directories still run side by side. A server that fails mid-request is
//! discarded and the caller falls back to an ordinary process, which reproduces
//! any real failure with the compiler's own exit status.
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

#[derive(Default)]
pub struct RustcServers {
    idle: std::sync::Mutex<Vec<Server>>,
    counter: std::sync::atomic::AtomicU64,
}

struct Server {
    driver: PathBuf,
    socket: PathBuf,
    requests: u32,
    // Held for its stdin: the server exits when that pipe closes.
    _child: Child,
    _stdin: tokio::process::ChildStdin,
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
        if let Some(request) = describe(&command, driver) {
            match self.serve(driver, &request, deadline).await {
                Ok(Some(output)) => return Ok(output),
                Ok(None) => {}
                Err(error) => return Err(error),
            }
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
            Ok(Err(_)) => return Ok(None),
            Err(_) => {
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
        let socket = std::env::temp_dir().join(format!(
            "loom-rustc-{}-{}.sock",
            std::process::id(),
            self.counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut command = Command::new(driver);
        command
            .arg("--loom-serve")
            .arg(&socket)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", std::env::var_os("HOME").unwrap_or_default())
            .envs(std::env::var_os("LOOM_DRIVER_TIMING").map(|value| ("LOOM_DRIVER_TIMING", value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().ok()?;
        let stdin = child.stdin.take()?;
        let mut ready = String::new();
        let stdout = child.stdout.take()?;
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            BufReader::new(stdout).read_line(&mut ready),
        )
        .await;
        if !matches!(read, Ok(Ok(count)) if count > 0) || ready.trim() != "ready" {
            return None;
        }
        Some(Server {
            driver: driver.to_owned(),
            socket,
            requests: 0,
            _child: child,
            _stdin: stdin,
        })
    }
}

impl Server {
    async fn exchange(&self, request: &Request<'_>) -> std::io::Result<Reply> {
        let mut stream = UnixStream::connect(&self.socket).await?;
        let mut line = serde_json::to_vec(request).map_err(std::io::Error::other)?;
        line.push(b'\n');
        stream.write_all(&line).await?;
        let mut reply = String::new();
        if BufReader::new(&mut stream).read_line(&mut reply).await? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        serde_json::from_str(&reply).map_err(std::io::Error::other)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
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
