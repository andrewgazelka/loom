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
    command: Command,
) -> anyhow::Result<bool> {
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
    Ok(status.is_success() && response.ok)
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
