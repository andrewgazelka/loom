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
    anyhow::ensure!(
        status != reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "{command}: the request is larger than the daemon accepts (16 MiB); import the definitions in smaller directories"
    );
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
            // `names` absent means all; the daemon takes an array or nothing, never null.
            let args = match &names {
                Some(names) => json!({"names": names}),
                None => json!({}),
            };
            let reply = ask(client, base, token, session, "export_defs", args).await?;
            let (docs, hashes) = documents(&reply)?;
            std::fs::create_dir_all(dir)?;
            let lock = Lock {
                toolchain: reply["toolchain"].as_str().unwrap_or_default().to_owned(),
                definitions: hashes,
            };
            loom_defdir::write_dir(dir, &docs)?;
            // A subset must not drop the lock entries of the definitions already in the directory.
            loom_defdir::write_lock(dir, &lock, names.is_some())?;
            for warning in reply["warnings"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                eprintln!("warning: {warning}");
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "dir": dir, "exported": docs.len(), "skipped": reply["skipped"],
                    "warnings": reply["warnings"], "toolchain": lock.toolchain,
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
                json!({
                    "definitions": docs,
                    "expected": lock.as_ref().map(|lock| &lock.definitions),
                    "toolchain": lock.as_ref().map(|lock| &lock.toolchain),
                }),
            )
            .await?;
            let failed = reply["failed"].as_u64().unwrap_or(0);
            for mismatch in reply["mismatches"].as_array().into_iter().flatten() {
                eprintln!(
                    "warning: {} hashes to {} here, loom.lock says {}",
                    mismatch["name"], mismatch["got"], mismatch["expected"]
                );
            }
            if !reply["toolchain_mismatch"].is_null() {
                eprintln!(
                    "warning: built with compiler {} but loom.lock was written with {}; different hashes are expected",
                    reply["toolchain_mismatch"]["got"], reply["toolchain_mismatch"]["expected"]
                );
            }
            println!("{}", serde_json::to_string_pretty(&reply)?);
            Ok(failed == 0)
        }
        "status-dir" => {
            let (docs, lock) = loom_defdir::read_dir(dir)?;
            let reply = ask(client, base, token, session, "export_defs", json!({})).await?;
            let (daemon, hashes) = documents(&reply)?;
            let rows = loom_defdir::status(&docs, &daemon, lock.as_ref(), &hashes);
            println!("{}", serde_json::to_string_pretty(&rows)?);
            // `removed` (on the daemon, not in DIR) is information: a shared daemon holds other definitions.
            // What fails the check is a directory that differs from the daemon.
            Ok(rows.iter().all(|row| matches!(row.state, "same" | "removed") && row.lock.is_none()))
        }
        other => anyhow::bail!("unknown command {other}"),
    }
}
