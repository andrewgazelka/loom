mod operation;

use anyhow::Context;
use operation::Command;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

fn main() -> anyhow::Result<std::process::ExitCode> {
    if let Some(status) = loom_build::compiler_cache_entry()? {
        return Ok(status);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(application())?;
    Ok(std::process::ExitCode::SUCCESS)
}

async fn application() -> anyhow::Result<()> {
    if let Err(error) = run().await {
        print_failure(&error);
        std::process::exit(1);
    }
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    let args = match operation::parser().try_get_matches() {
        Ok(args) => args,
        Err(error) if display_request(&error) => {
            error.print()?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let url = args.get_one::<String>("url").context("missing URL")?;
    let session = args.get_one::<String>("session").map(String::as_str);
    let token = args
        .get_one::<String>("token")
        .map(String::as_str)
        .filter(|token| !token.is_empty())
        .context("provide --token or set LOOM_TOKEN")?;
    let client = reqwest::Client::new();
    if let Some(operation) = operation::from_matches(&args)? {
        let accepted = execute(&client, url, token, session, operation).await?;
        if !accepted {
            std::process::exit(1);
        }
        return Ok(());
    }
    let mut input = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    loop {
        output.write_all(b"loom> ").await?;
        output.flush().await?;
        let Some(line) = input.next_line().await? else {
            break;
        };
        if matches!(line.trim(), ":quit" | "quit") {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let words = if matches!(line.trim(), ":help" | "help") {
            vec!["--help".to_owned()]
        } else {
            let Some(words) = shlex::split(&line) else {
                print_failure(&anyhow::anyhow!("unclosed quote in command"));
                continue;
            };
            words
        };
        let command = match operation::parser()
            .try_get_matches_from(std::iter::once("loom".to_owned()).chain(words))
        {
            Ok(command) => command,
            Err(error) => {
                if display_request(&error) {
                    error.print()?;
                } else {
                    print_failure(&error.into());
                }
                continue;
            }
        };
        let command = match operation::from_matches(&command) {
            Ok(Some(command)) => command,
            Ok(None) => continue,
            Err(error) => {
                print_failure(&error);
                continue;
            }
        };
        if let Err(error) = execute(&client, url, token, session, command).await {
            print_failure(&error);
        }
    }
    Ok(())
}

async fn execute(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    session: Option<&str>,
    mut command: Command,
) -> anyhow::Result<bool> {
    let base = url.trim_end_matches('/');
    // `--out` is claimed before any request: an existing file refuses the
    // export up front instead of after the server has done the work.
    let mut reservation = match &command.download {
        Some(path) => Some(Reservation::claim(path)?),
        None => None,
    };
    for upload in &command.uploads {
        let cid = upload_file(client, base, token, &upload.path).await?;
        command.args[upload.argument] = serde_json::Value::String(cid);
    }
    let response = client
        .post(format!("{base}/v1/command"))
        .bearer_auth(token)
        .json(&serde_json::json!({"session":session,"command":command.name,"args":command.args}))
        .send()
        .await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    let mut response: loom_proto::Response = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "HTTP {status}: {error}: {}",
            String::from_utf8_lossy(&bytes)
        )
    })?;
    let accepted = status.is_success() && response.ok;
    if accepted && let Some(reservation) = reservation.take() {
        let cid = response.result["bundle"]["$ref"]
            .as_str()
            .context("export response has no bundle reference")?;
        let path = reservation.path.display().to_string();
        let written = reservation.fill(client, base, token, cid).await?;
        response.result["out"] = serde_json::json!(path);
        response.result["out_bytes"] = serde_json::json!(written);
    }
    // A reservation still held here (request refused) is removed by its Drop.
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(accepted)
}

/// An `--out` path claimed with `create_new` before the export request, so an
/// existing file refuses the export before the server does any work. The
/// bytes go to `<out>.tmp` and are renamed over the claim in one step; until
/// that rename, dropping the reservation removes the claim (and any partial
/// temporary), so `--out` is either absent or the complete bundle.
struct Reservation {
    path: std::path::PathBuf,
    kept: bool,
}
impl Reservation {
    fn claim(path: &std::path::Path) -> anyhow::Result<Self> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("create {}", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            kept: false,
        })
    }
    /// Fetch the CAS object and move it into place; returns the byte count.
    async fn fill(
        mut self,
        client: &reqwest::Client,
        base: &str,
        token: &str,
        cid: &str,
    ) -> anyhow::Result<usize> {
        let response = client
            .get(format!("{base}/v1/cas/{cid}"))
            .bearer_auth(token)
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        anyhow::ensure!(
            status.is_success(),
            "download {cid}: HTTP {status}: {}",
            String::from_utf8_lossy(&bytes)
        );
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".tmp");
        let temporary = std::path::PathBuf::from(temporary);
        let written = write_then_rename(&temporary, &self.path, &bytes);
        if written.is_err() {
            // Best effort: the error being returned is the one that matters.
            let _ = std::fs::remove_file(&temporary);
        }
        written?;
        self.kept = true;
        Ok(bytes.len())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.kept {
            // Best effort: an empty claim left behind is the only consequence.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Write `bytes` to a fresh `temporary` (an existing one is refused, never
/// reused), sync, then rename it over `path`.
fn write_then_rename(
    temporary: &std::path::Path,
    path: &std::path::Path,
    bytes: &[u8],
) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    std::io::Write::write_all(&mut file, bytes)
        .with_context(|| format!("write {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", temporary.display()))?;
    drop(file);
    std::fs::rename(temporary, path)
        .with_context(|| format!("rename {} to {}", temporary.display(), path.display()))
}

/// Store a local file as a raw CAS object and return its CID.
async fn upload_file(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &std::path::Path,
) -> anyhow::Result<String> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let response = client
        .post(format!("{base}/v1/cas"))
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(bytes)
        .send()
        .await?;
    let status = response.status();
    let body = response.bytes().await?;
    anyhow::ensure!(
        status.is_success(),
        "upload {}: HTTP {status}: {}",
        path.display(),
        String::from_utf8_lossy(&body)
    );
    let reply: serde_json::Value = serde_json::from_slice(&body)
        .with_context(|| format!("upload {}: invalid reply", path.display()))?;
    reply["$ref"]
        .as_str()
        .map(str::to_owned)
        .context("upload reply has no $ref")
}

fn print_failure(error: &anyhow::Error) {
    println!(
        "{}",
        serde_json::json!({"ok":false,"seq":0,"result":{"error":format!("{error:#}")},"diagnostics":[]})
    );
}

fn display_request(error: &clap::Error) -> bool {
    matches!(
        error.kind(),
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
    )
}
