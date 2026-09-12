mod operation;

use anyhow::Context;
use clap::Parser;
use operation::Operation;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

#[derive(Parser)]
#[command(about = "Content-addressed Rust definitions and actors")]
struct Args {
    #[command(subcommand)]
    operation: Option<Operation>,
    #[arg(long, default_value = "http://127.0.0.1:8787", global = true)]
    url: String,
    #[arg(long, env = "LOOM_TOKEN", global = true)]
    token: Option<String>,
    #[arg(long, global = true)]
    session: Option<String>,
}
#[derive(Parser)]
struct ReplLine {
    #[command(subcommand)]
    operation: Operation,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let token = args
        .token
        .as_deref()
        .filter(|token| !token.is_empty())
        .context("provide --token or set LOOM_TOKEN")?;
    let client = reqwest::Client::new();
    if let Some(operation) = args.operation {
        return execute(
            &client,
            &args.url,
            token,
            args.session.as_deref(),
            operation,
        )
        .await;
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
                eprintln!("unclosed quote in command");
                continue;
            };
            words
        };
        let command =
            match ReplLine::try_parse_from(std::iter::once("loom".to_owned()).chain(words)) {
                Ok(command) => command,
                Err(error) => {
                    error.print()?;
                    continue;
                }
            };
        if let Err(error) = execute(
            &client,
            &args.url,
            token,
            args.session.as_deref(),
            command.operation,
        )
        .await
        {
            eprintln!("{error:#}");
        }
    }
    Ok(())
}

async fn execute(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    session: Option<&str>,
    operation: Operation,
) -> anyhow::Result<()> {
    let command = operation.command()?;
    let response = client
        .post(format!("{}/v1/command", url.trim_end_matches('/')))
        .bearer_auth(token)
        .json(&serde_json::json!({"session":session,"command":command.name,"args":command.args}))
        .send()
        .await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    let response: loom_proto::Response = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "HTTP {status}: {error}: {}",
            String::from_utf8_lossy(&bytes)
        )
    })?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    anyhow::ensure!(status.is_success() && response.ok, "operation rejected");
    Ok(())
}
