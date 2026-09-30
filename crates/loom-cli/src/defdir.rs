//! `loom export-dir`, `import-dir` and `status-dir`: definitions as a directory of files that goes in git
//! (`docs/design/definitions-in-git.md`). The daemon speaks documents (`export_defs`, `import_defs`); this
//! module is where they meet the filesystem, so the server never reads or writes the client's files.
use anyhow::{Context, Result};
use loom_defdir::{Doc, Lock};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

pub const COMMANDS: [&str; 3] = ["export-dir", "import-dir", "status-dir"];

pub fn parsers() -> Vec<clap::Command> {
    let dir = || clap::Arg::new("dir").required(true).value_name("DIR");
    vec![
        clap::Command::new("export-dir")
            .about("Write the daemon's Rust definitions as files (lib.rs, def.toml, loom.lock) under DIR")
            .arg(dir())
            .arg(clap::Arg::new("names").num_args(0..).value_name("NAME").help("Only these definitions")),
        clap::Command::new("import-dir")
            .about("Add, update or skip the definitions under DIR, dependencies first; report lock mismatches")
            .arg(dir()),
        clap::Command::new("status-dir")
            .about("Compare DIR with the daemon: same, changed, new or removed per definition")
            .arg(dir()),
    ]
}

async fn ask(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    session: Option<&str>,
    command: &str,
    args: Value,
) -> Result<Value> {
    let response = client
        .post(format!("{}/v1/command", base.trim_end_matches('/')))
        .bearer_auth(token)
        .json(&json!({"session": session, "command": command, "args": args}))
        .send()
        .await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    let reply: loom_proto::Response = serde_json::from_slice(&bytes)
        .with_context(|| format!("HTTP {status}: {}", String::from_utf8_lossy(&bytes)))?;
    anyhow::ensure!(
        status.is_success() && reply.ok,
        "{command} failed: {}",
        serde_json::to_string(&reply.result).unwrap_or_default()
    );
    Ok(reply.result)
}

/// The documents and their hashes in an `export_defs` reply.
fn documents(reply: &Value) -> Result<(Vec<Doc>, BTreeMap<String, String>)> {
    let mut docs = Vec::new();
    let mut hashes = BTreeMap::new();
    for item in reply["definitions"].as_array().context("export_defs returned no definitions")? {
        let doc: Doc = serde_json::from_value(item.clone())?;
        hashes.insert(doc.name.clone(), item["hash"].as_str().unwrap_or_default().to_owned());
        docs.push(doc);
    }
    Ok((docs, hashes))
}

pub async fn run(
    name: &str,
    matches: &clap::ArgMatches,
    client: &reqwest::Client,
    base: &str,
    token: &str,
    session: Option<&str>,
) -> Result<bool> {
    let dir = Path::new(matches.get_one::<String>("dir").context("missing DIR")?);
    match name {
        "export-dir" => {
            let names: Option<Vec<String>> = matches
                .get_many::<String>("names")
                .map(|names| names.cloned().collect());
            let reply = ask(client, base, token, session, "export_defs", json!({"names": names})).await?;
            let (docs, hashes) = documents(&reply)?;
            std::fs::create_dir_all(dir)?;
            let lock = Lock {
                toolchain: reply["toolchain"].as_str().unwrap_or_default().to_owned(),
                definitions: hashes,
            };
            loom_defdir::write_dir(dir, &docs, Some(&lock))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "dir": dir, "exported": docs.len(), "skipped": reply["skipped"], "toolchain": lock.toolchain,
                }))?
            );
            Ok(true)
        }
        "import-dir" => {
            let (docs, lock) = loom_defdir::read_dir(dir)?;
            let reply = ask(
                client,
                base,
                token,
                session,
                "import_defs",
                json!({"definitions": docs, "expected": lock.as_ref().map(|lock| &lock.definitions)}),
            )
            .await?;
            let failed = reply["failed"].as_u64().unwrap_or(0);
            println!("{}", serde_json::to_string_pretty(&reply)?);
            Ok(failed == 0)
        }
        "status-dir" => {
            let (docs, lock) = loom_defdir::read_dir(dir)?;
            let reply = ask(client, base, token, session, "export_defs", json!({})).await?;
            let (daemon, hashes) = documents(&reply)?;
            let rows = loom_defdir::status(&docs, &daemon, lock.as_ref(), &hashes);
            println!("{}", serde_json::to_string_pretty(&rows)?);
            Ok(rows.iter().all(|row| row.state == "same" && row.lock.is_none()))
        }
        other => anyhow::bail!("unknown command {other}"),
    }
}
