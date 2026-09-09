use clap::Parser;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:8787")]
    url: String,
    #[arg(long, env = "LOOM_TOKEN")]
    token: String,
    #[arg(long)]
    session: Option<String>,
    #[arg(long)]
    eval: Option<String>,
    #[arg(long, conflicts_with_all=["eval","define"])]
    command: Option<String>,
    #[arg(long, conflicts_with = "eval", help = "Definition request as JSON")]
    define: Option<String>,
    #[arg(long, default_value = "{}")]
    args: String,
    #[arg(
        long,
        default_value = "{}",
        help = "Definition aliases as JSON name-to-hash mapping"
    )]
    deps: String,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let client = reqwest::Client::new();
    let deps: std::collections::BTreeMap<String, String> = serde_json::from_str(&args.deps)?;
    if let Some(define) = args.define {
        return print_response(
            &client,
            &args.url,
            &args.token,
            "define",
            serde_json::to_value(serde_json::from_str::<loom_proto::DefineRequest>(&define)?)?,
        )
        .await;
    }
    if let Some(command) = args.command {
        let body = serde_json::json!({"session":args.session,"command":command,"args":serde_json::from_str::<serde_json::Value>(&args.args)?});
        return print_response(&client, &args.url, &args.token, "command", body).await;
    }
    if let Some(source) = args.eval {
        return print_response(
            &client,
            &args.url,
            &args.token,
            "eval",
            serde_json::json!({"session":args.session,"source":source,"deps":deps}),
        )
        .await;
    }
    let mut session = args.session;
    let mut input = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    loop {
        output.write_all(b"loom> ").await?;
        output.flush().await?;
        let Some(source) = input.next_line().await? else {
            break;
        };
        if source.trim() == ":quit" {
            break;
        }
        if source.trim().is_empty() {
            continue;
        }
        if source.trim() == ":help" {
            println!(
                "Enter a TS expression, :define <JSON>, :command <JSON>, or :quit. Definitions use {{name,lang,source,deps}}; commands use {{command,args}}."
            );
            continue;
        }
        let request = if let Some(body) = source.strip_prefix(":define ") {
            serde_json::from_str::<loom_proto::DefineRequest>(body)
                .and_then(serde_json::to_value)
                .map(|body| ReplRequest {
                    operation: "define",
                    body,
                })
        } else if let Some(body) = source.strip_prefix(":command ") {
            serde_json::from_str::<loom_proto::CommandRequest>(body)
                .and_then(serde_json::to_value)
                .map(|body| ReplRequest {
                    operation: "command",
                    body,
                })
        } else {
            Ok(ReplRequest {
                operation: "eval",
                body: serde_json::json!({"session":session,"source":source,"deps":deps}),
            })
        };
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                eprintln!("{error}");
                continue;
            }
        };
        let response = match send(
            &client,
            &args.url,
            &args.token,
            request.operation,
            request.body,
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                eprintln!("{error:#}");
                continue;
            }
        };
        if let Some(id) = response.result["session"].as_str() {
            session = Some(id.to_string())
        }
        println!("{}", serde_json::to_string_pretty(&response)?);
    }
    Ok(())
}
async fn print_response(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    operation: &str,
    body: serde_json::Value,
) -> anyhow::Result<()> {
    let response = send(client, url, token, operation, body).await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    anyhow::ensure!(response.ok, "operation rejected");
    Ok(())
}

struct ReplRequest {
    operation: &'static str,
    body: serde_json::Value,
}
async fn send(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    operation: &str,
    body: serde_json::Value,
) -> anyhow::Result<loom_proto::Response> {
    let response = client
        .post(format!("{url}/v1/{operation}"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    match serde_json::from_slice(&bytes) {
        Ok(response) => Ok(response),
        Err(error) => Err(anyhow::anyhow!(
            "HTTP {status}: {error}: {}",
            String::from_utf8_lossy(&bytes)
        )),
    }
}
