use anyhow::Context;
use loom_proto::verbs::{Kind, VERBS};
use serde_json::Value;

pub struct Command {
    pub name: &'static str,
    pub args: Value,
}

pub fn parser() -> clap::Command {
    let mut parser = clap::Command::new("loom")
        .about("Content-addressed Rust definitions and actors")
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
            command = command.arg(arg);
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
    for argument in verb.arguments {
        let Some(input) = matches.get_one::<String>(argument.name) else {
            continue;
        };
        let value = match argument.kind {
            Kind::String => Value::String(input.clone()),
            Kind::Source => Value::String(
                std::fs::read_to_string(input)
                    .with_context(|| format!("read definition {input}"))?,
            ),
            Kind::Json | Kind::Integer | Kind::Count => serde_json::from_str(input)
                .with_context(|| format!("invalid JSON argument {}", argument.name))?,
        };
        arguments.insert(argument.name.into(), value);
    }
    let mut args = Value::Object(arguments);
    verb.normalize(&mut args).map_err(anyhow::Error::msg)?;
    Ok(Some(Command {
        name: verb.name,
        args,
    }))
}
