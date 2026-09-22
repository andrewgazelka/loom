use anyhow::Context;
use loom_proto::verbs::{Kind, VERBS};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug)]
pub struct Command {
    pub name: &'static str,
    pub args: Value,
    /// `Upload` arguments: the file at `path` is stored through `POST /v1/cas`
    /// and `args[argument]` (holding the path until then) becomes its CID.
    pub uploads: Vec<Upload>,
    /// `export --out`: where the CLI writes the bundle named by `result.bundle`.
    pub download: Option<PathBuf>,
}
#[derive(Debug)]
pub struct Upload {
    pub argument: &'static str,
    pub path: PathBuf,
}

/// The flag spelling for a `Map` argument's repeated pairs: `--deps a=b` and
/// its singular alias `--dep a=b`.
fn singular(name: &'static str) -> Option<&'static str> {
    name.strip_suffix('s').filter(|stem| !stem.is_empty())
}

pub fn parser() -> clap::Command {
    let mut parser = clap::Command::new("loom")
        .about("Content-addressed TypeScript, JavaScript and Rust definitions and actors")
        .arg(
            clap::Arg::new("url")
                .long("url")
                .default_value("http://127.0.0.1:8787")
                .global(true),
        )
        .arg(
            clap::Arg::new("token")
                .long("token")
                .env("LOOM_TOKEN")
                .global(true),
        )
        .arg(clap::Arg::new("session").long("session").global(true));
    for verb in VERBS {
        let mut command = clap::Command::new(verb.name);
        for argument in verb.arguments {
            let mut arg = clap::Arg::new(argument.name).required(argument.required);
            if argument.flag && !(verb.name == "view" && argument.name == "target") {
                arg = arg.long(argument.name);
            }
            if verb.name == "add" && argument.name == "lang" {
                arg = arg.help("Source language: typescript (default), javascript, or rust");
            }
            match argument.kind {
                Kind::Boolean => arg = arg.action(clap::ArgAction::SetTrue),
                Kind::Map => {
                    arg = arg
                        .action(clap::ArgAction::Append)
                        .value_name("KEY=VALUE")
                        .help("Repeatable key=value pair; a value is a definition name or hash");
                    if let Some(alias) = singular(argument.name) {
                        arg = arg.visible_alias(alias);
                    }
                }
                Kind::List => arg = arg.num_args(1..).value_name(argument.name),
                Kind::Upload => arg = arg.value_name("FILE").help("File uploaded to the CAS"),
                Kind::Source => arg = arg.value_name("FILE"),
                Kind::String | Kind::Json | Kind::Integer | Kind::Count => {}
            }
            command = command.arg(arg);
        }
        if verb.name == "export" {
            command = command.arg(
                clap::Arg::new("out")
                    .long("out")
                    .required(true)
                    .value_name("PATH")
                    .help("Write the bundle here; the file must not exist"),
            );
        }
        parser = parser.subcommand(command);
    }
    parser
}

pub fn from_matches(matches: &clap::ArgMatches) -> anyhow::Result<Option<Command>> {
    let Some((name, matches)) = matches.subcommand() else {
        return Ok(None);
    };
    let verb = loom_proto::verbs::lookup(name).context("unknown command")?;
    let mut arguments = serde_json::Map::new();
    let mut uploads = Vec::new();
    for argument in verb.arguments {
        match argument.kind {
            Kind::Boolean => {
                arguments.insert(
                    argument.name.into(),
                    Value::Bool(matches.get_flag(argument.name)),
                );
                continue;
            }
            Kind::Map => {
                let Some(pairs) = matches.get_many::<String>(argument.name) else {
                    continue;
                };
                let mut map = serde_json::Map::new();
                for pair in pairs {
                    let (key, value) = pair.split_once('=').with_context(|| {
                        format!("--{} expects KEY=VALUE, got {pair:?}", argument.name)
                    })?;
                    anyhow::ensure!(
                        !key.is_empty() && !value.is_empty(),
                        "--{} expects KEY=VALUE, got {pair:?}",
                        argument.name
                    );
                    anyhow::ensure!(
                        map.insert(key.into(), Value::String(value.into()))
                            .is_none(),
                        "--{} names {key:?} twice",
                        argument.name
                    );
                }
                arguments.insert(argument.name.into(), Value::Object(map));
                continue;
            }
            Kind::List => {
                let Some(words) = matches.get_many::<String>(argument.name) else {
                    continue;
                };
                arguments.insert(
                    argument.name.into(),
                    Value::Array(words.cloned().map(Value::String).collect()),
                );
                continue;
            }
            Kind::String
            | Kind::Source
            | Kind::Json
            | Kind::Integer
            | Kind::Count
            | Kind::Upload => {}
        }
        let Some(input) = matches.get_one::<String>(argument.name) else {
            continue;
        };
        let value = match argument.kind {
            Kind::String => Value::String(input.clone()),
            Kind::Source => Value::String(
                std::fs::read_to_string(input)
                    .with_context(|| format!("read definition {input}"))?,
            ),
            Kind::Upload => {
                let path = PathBuf::from(input);
                anyhow::ensure!(path.is_file(), "{input} is not a file");
                uploads.push(Upload {
                    argument: argument.name,
                    path,
                });
                Value::String(input.clone())
            }
            Kind::Json | Kind::Integer | Kind::Count => serde_json::from_str(input)
                .with_context(|| format!("invalid JSON argument {}", argument.name))?,
            Kind::Boolean | Kind::Map | Kind::List => unreachable!("collected above"),
        };
        arguments.insert(argument.name.into(), value);
    }
    let mut args = Value::Object(arguments);
    verb.normalize(&mut args).map_err(anyhow::Error::msg)?;
    let download = if verb.name == "export" {
        Some(PathBuf::from(
            matches
                .get_one::<String>("out")
                .context("export requires --out")?,
        ))
    } else {
        None
    };
    Ok(Some(Command {
        name: verb.name,
        args,
        uploads,
        download,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_defaults_to_typescript_and_explicit_language_controls_any_filename() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.rs");
        let source = "function main(value: number): number { return value; }\n";
        std::fs::write(&path, source).unwrap();
        let path = path.to_str().unwrap();
        let matches = parser()
            .try_get_matches_from(["loom", "add", path])
            .unwrap();
        let command = from_matches(&matches).unwrap().unwrap();
        assert_eq!(command.args["lang"], "typescript");
        assert_eq!(command.args["source"], source);
        for lang in ["rust", "javascript"] {
            let matches = parser()
                .try_get_matches_from(["loom", "add", path, "--lang", lang])
                .unwrap();
            let command = from_matches(&matches).unwrap().unwrap();
            assert_eq!(command.args["lang"], lang);
            assert_eq!(command.args["source"], source);
        }
    }

    #[test]
    fn dependency_pins_collect_repeated_pairs_under_both_spellings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("caller.rs");
        std::fs::write(&path, "pub fn main() -> i32 { util::twice(21) }").unwrap();
        let path = path.to_str().unwrap();
        let hash = "ab".repeat(32);
        let pinned = format!("pinned={hash}");
        for flag in ["--dep", "--deps"] {
            let matches = parser()
                .try_get_matches_from([
                    "loom",
                    "add",
                    path,
                    "--lang",
                    "rust",
                    flag,
                    "util=util",
                    flag,
                    pinned.as_str(),
                ])
                .unwrap();
            let command = from_matches(&matches).unwrap().unwrap();
            assert_eq!(
                command.args["deps"],
                serde_json::json!({"util": "util", "pinned": hash})
            );
            assert!(command.uploads.is_empty() && command.download.is_none());
        }
        let matches = parser()
            .try_get_matches_from(["loom", "update", "caller", path, "--dep", "util=util"])
            .unwrap();
        assert_eq!(
            from_matches(&matches).unwrap().unwrap().args["deps"],
            serde_json::json!({"util": "util"})
        );
        for invalid in ["util", "=util", "util="] {
            let matches = parser()
                .try_get_matches_from(["loom", "add", path, "--dep", invalid])
                .unwrap();
            let error = from_matches(&matches).unwrap_err().to_string();
            assert!(error.contains("KEY=VALUE"), "{invalid}: {error}");
        }
        let matches = parser()
            .try_get_matches_from(["loom", "add", path, "--dep", "util=a", "--dep", "util=b"])
            .unwrap();
        assert!(
            from_matches(&matches)
                .unwrap_err()
                .to_string()
                .contains("twice")
        );
        let matches = parser()
            .try_get_matches_from(["loom", "add", path, "--lang", "rust"])
            .unwrap();
        assert!(
            from_matches(&matches)
                .unwrap()
                .unwrap()
                .args
                .get("deps")
                .is_none()
        );
    }

    #[test]
    fn export_collects_targets_and_import_uploads_the_bundle_file() {
        let directory = tempfile::tempdir().unwrap();
        let out = directory.path().join("greet.car");
        let matches = parser()
            .try_get_matches_from([
                "loom",
                "export",
                "greet",
                "util",
                "--out",
                out.to_str().unwrap(),
            ])
            .unwrap();
        let command = from_matches(&matches).unwrap().unwrap();
        assert_eq!(command.name, "export");
        assert_eq!(
            command.args,
            serde_json::json!({"targets": ["greet", "util"]})
        );
        assert_eq!(command.download.as_deref(), Some(out.as_path()));
        assert!(
            parser()
                .try_get_matches_from(["loom", "export", "greet"])
                .is_err()
        );
        assert!(
            parser()
                .try_get_matches_from(["loom", "export", "--out", "x"])
                .is_err()
        );
        let bundle = directory.path().join("bundle.car");
        std::fs::write(&bundle, b"not yet a bundle").unwrap();
        let matches = parser()
            .try_get_matches_from([
                "loom",
                "import",
                bundle.to_str().unwrap(),
                "--into",
                "friend",
            ])
            .unwrap();
        let command = from_matches(&matches).unwrap().unwrap();
        assert_eq!(command.name, "import");
        assert_eq!(command.args["into"], "friend");
        assert_eq!(command.args["bundle"], bundle.to_str().unwrap());
        assert_eq!(command.uploads.len(), 1);
        assert_eq!(command.uploads[0].argument, "bundle");
        assert_eq!(command.uploads[0].path, bundle);
        let matches = parser()
            .try_get_matches_from(["loom", "import", "/nonexistent/bundle.car"])
            .unwrap();
        assert!(from_matches(&matches).is_err());
    }

    #[test]
    fn cluster_commands_share_the_wire_vocabulary() {
        let matches = parser()
            .try_get_matches_from(["loom", "actors", "--cluster"])
            .unwrap();
        let command = from_matches(&matches).unwrap().unwrap();
        assert_eq!(command.name, "actors");
        assert_eq!(command.args, serde_json::json!({"cluster": true}));
        let matches = parser().try_get_matches_from(["loom", "actors"]).unwrap();
        assert_eq!(
            from_matches(&matches).unwrap().unwrap().args,
            serde_json::json!({"cluster": false})
        );
        let matches = parser()
            .try_get_matches_from(["loom", "move", "a0actor", "node2"])
            .unwrap();
        let command = from_matches(&matches).unwrap().unwrap();
        assert_eq!(command.name, "move");
        assert_eq!(
            command.args,
            serde_json::json!({"id": "a0actor", "node_id": "node2"})
        );
        let matches = parser().try_get_matches_from(["loom", "nodes"]).unwrap();
        assert_eq!(from_matches(&matches).unwrap().unwrap().name, "nodes");
    }
}
