use anyhow::Context;
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(clap::Subcommand)]
pub enum Operation {
    Add {
        file: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "{}")]
        deps: String,
        #[arg(long)]
        allowed_effects: Option<String>,
    },
    View {
        target: String,
    },
    Update {
        name: String,
        file: PathBuf,
        #[arg(long)]
        deps: Option<String>,
        #[arg(long)]
        allowed_effects: Option<String>,
    },
    History {
        name: String,
    },
    Diff {
        old: String,
        new: String,
    },
    Run {
        target: String,
        #[arg(default_value = "[]")]
        args: String,
    },
    Find {
        text: String,
    },
    Dependents {
        hash: String,
    },
    Spawn {
        def: String,
        #[arg(default_value = "null")]
        init: String,
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        spec: Option<String>,
    },
    Send {
        id: String,
        msg: String,
        #[arg(long)]
        key: Option<String>,
    },
    Tree {
        #[arg(long)]
        root: Option<String>,
    },
    Info {
        id: String,
    },
    Lineage {
        id: String,
    },
    Validate {
        id: String,
        candidate: String,
        k: u64,
        #[arg(long)]
        assertions: Option<String>,
    },
    Promote {
        id: String,
        hash: String,
        #[arg(long)]
        rationale: String,
        #[arg(long, default_value = "cli")]
        author: String,
    },
    Fork {
        id: String,
        seq: i64,
    },
    Actors,
}

pub struct Command {
    pub name: &'static str,
    pub args: Value,
}
impl Operation {
    pub fn command(self) -> anyhow::Result<Command> {
        let command = match self {
            Self::Add {
                file,
                name,
                deps,
                allowed_effects,
            } => Command {
                name: "add",
                args: json!({"source":source(&file)?,"name":name,"deps":parse(&deps)?,"allowed_effects":allowed_effects.map(|value|parse(&value)).transpose()?}),
            },
            Self::View { target } => Command {
                name: "view",
                args: json!({"target":target}),
            },
            Self::Update {
                name,
                file,
                deps,
                allowed_effects,
            } => {
                let mut args = json!({"name":name,"source":source(&file)?});
                if let Some(deps) = deps {
                    args["deps"] = parse(&deps)?;
                }
                if let Some(allowed_effects) = allowed_effects {
                    args["allowed_effects"] = parse(&allowed_effects)?;
                }
                Command {
                    name: "update",
                    args,
                }
            }
            Self::History { name } => Command {
                name: "history",
                args: json!({"name":name}),
            },
            Self::Diff { old, new } => Command {
                name: "diff",
                args: json!({"old":old,"new":new}),
            },
            Self::Run { target, args } => Command {
                name: "run",
                args: json!({"target":target,"args":parse(&args)?}),
            },
            Self::Find { text } => Command {
                name: "find",
                args: json!({"text":text}),
            },
            Self::Dependents { hash } => Command {
                name: "dependents",
                args: json!({"hash":hash}),
            },
            Self::Spawn {
                def,
                init,
                parent,
                spec,
            } => Command {
                name: "actor_spawn",
                args: json!({"behavior_hash":def,"init":parse(&init)?,"parent":parent,"spec":spec.map(|value|parse(&value)).transpose()?}),
            },
            Self::Send { id, msg, key } => Command {
                name: "actor_send",
                args: json!({"id":id,"msg":parse(&msg)?,"key":key}),
            },
            Self::Tree { root } => Command {
                name: "actor_tree",
                args: json!({"root":root}),
            },
            Self::Info { id } => Command {
                name: "actor_info",
                args: json!({"id":id}),
            },
            Self::Lineage { id } => Command {
                name: "actor_lineage",
                args: json!({"id":id}),
            },
            Self::Validate {
                id,
                candidate,
                k,
                assertions,
            } => Command {
                name: "actor_validate",
                args: json!({"id":id,"candidate_hash":candidate,"k":k,"assertions":assertions.map(|value|parse(&value)).transpose()?.unwrap_or(json!([]))}),
            },
            Self::Promote {
                id,
                hash,
                rationale,
                author,
            } => Command {
                name: "actor_promote",
                args: json!({"id":id,"behavior_hash":hash,"rationale":rationale,"author":author}),
            },
            Self::Fork { id, seq } => Command {
                name: "actor_fork",
                args: json!({"id":id,"at_seq":seq}),
            },
            Self::Actors => Command {
                name: "actor_list",
                args: json!({}),
            },
        };
        Ok(command)
    }
}
fn parse(input: &str) -> anyhow::Result<Value> {
    serde_json::from_str(input).context("invalid JSON argument")
}
fn source(file: &std::path::Path) -> anyhow::Result<String> {
    std::fs::read_to_string(file).with_context(|| format!("read definition {}", file.display()))
}
