use crate::wire::{self, Descriptor, Mode, parse, value};
use loom_actor::{Ctx, RestartVerb, Trap, Value as SqlValue};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    target: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    target: String,
    msg: Vec<u8>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stop {
    target: String,
    reason: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reason {
    reason: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    reference: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Demonitor {
    reference: String,
    flush: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrapExit {
    enabled: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Restart {
    target: String,
    verb: RestartVerb,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendAfter {
    target: String,
    ms: u64,
    msg: Vec<u8>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Call {
    target: String,
    msg: Vec<u8>,
    timeout_ms: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    from: String,
    reference: String,
    msg: Vec<u8>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Random {
    n: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Host {
    request: Vec<u8>,
    #[serde(default)]
    mode: Mode,
}

pub async fn dispatch(
    cx: &mut Ctx<'_>,
    definition: &str,
    descriptor: Value,
) -> Result<Value, Trap> {
    let name = descriptor
        .get("op")
        .and_then(Value::as_str)
        .unwrap_or("<missing op>")
        .to_owned();
    let Descriptor { op, args } = parse(&name, descriptor)?;
    if op.is_empty() {
        return Err(Trap::new("effect op must not be empty"));
    }
    match op.as_str() {
        "sql" => {
            let request: wire::Sql = parse(&op, args)?;
            let params: Vec<SqlValue> = request.params.into_iter().map(Into::into).collect();
            let rows = cx.sql(&request.sql, params).await?;
            wire::rows(cx, rows)
        }
        "actor.send" => {
            let request: Send = parse(&op, args)?;
            cx.send(&request.target, &request.msg).await?;
            Ok(Value::Null)
        }
        "actor.spawn" => {
            let mut spec = wire::child_spec(args)?;
            if spec.behavior_hash == "$self" {
                spec.behavior_hash = definition.into();
            }
            value(cx.spawn(&spec).await?)
        }
        "actor.stop" => {
            let request: Stop = parse(&op, args)?;
            cx.stop(&request.target, &request.reason).await?;
            Ok(Value::Null)
        }
        "actor.monitor" => {
            let request: Target = parse(&op, args)?;
            value(cx.monitor(&request.target).await?)
        }
        "actor.demonitor" => {
            let request: Demonitor = parse(&op, args)?;
            cx.demonitor(&request.reference, request.flush).await?;
            Ok(Value::Null)
        }
        "actor.link" => {
            let request: Target = parse(&op, args)?;
            cx.link(&request.target).await?;
            Ok(Value::Null)
        }
        "actor.unlink" => {
            let request: Target = parse(&op, args)?;
            cx.unlink(&request.target).await?;
            Ok(Value::Null)
        }
        "actor.shutdown" => {
            let request: Target = parse(&op, args)?;
            cx.shutdown(&request.target).await?;
            Ok(Value::Null)
        }
        "actor.exit" => {
            let request: Reason = parse(&op, args)?;
            cx.exit(&request.reason).await?;
            Ok(Value::Null)
        }
        "actor.trap_exit" => {
            let request: TrapExit = parse(&op, args)?;
            cx.trap_exit(request.enabled).await?;
            Ok(Value::Null)
        }
        "actor.restart" => {
            let request: Restart = parse(&op, args)?;
            cx.restart(&request.target, request.verb).await?;
            Ok(Value::Null)
        }
        "actor.inspect" => {
            let request: Target = parse(&op, args)?;
            value(cx.inspect(&request.target).await?)
        }
        "actor.send_after" => {
            let request: SendAfter = parse(&op, args)?;
            value(
                cx.send_after(&request.target, request.ms, &request.msg)
                    .await?,
            )
        }
        "actor.cancel_timer" => {
            let request: Reference = parse(&op, args)?;
            cx.cancel_timer(&request.reference).await?;
            Ok(Value::Null)
        }
        "actor.read_timer" => {
            let request: Reference = parse(&op, args)?;
            value(cx.read_timer(&request.reference).await?)
        }
        "actor.call" => {
            let request: Call = parse(&op, args)?;
            value(
                cx.call(&request.target, &request.msg, request.timeout_ms)
                    .await?,
            )
        }
        "actor.reply" => {
            let request: Reply = parse(&op, args)?;
            cx.reply(&request.from, &request.reference, &request.msg)
                .await?;
            Ok(Value::Null)
        }
        "random" => {
            let request: Random = parse(&op, args)?;
            if request.n > loom_proto::TRACE_MAX_BLOB_BYTES / 2 {
                return Err(Trap::new("effect random: n exceeds guest result limit"));
            }
            value(cx.random(request.n))
        }
        "actor.seq" | "actor.self_id" | "actor.sender" | "actor.defer" | "now" => {
            parse::<()>(&op, args)?;
            match op.as_str() {
                "actor.seq" => value(cx.seq()),
                "actor.self_id" => value(cx.self_id()),
                "actor.sender" => value(cx.sender()),
                "actor.defer" => {
                    cx.defer()?;
                    Ok(Value::Null)
                }
                _ => value(cx.now().await?),
            }
        }
        _ if op.starts_with("actor.") => Err(Trap::new(format!("unknown effect {op}"))),
        _ => {
            let request: Host = parse(&op, args)?;
            if request.mode == Mode::Request {
                value(cx.request(&op, &request.request).await?)
            } else {
                value(cx.effect(&op, &request.request).await?)
            }
        }
    }
}
