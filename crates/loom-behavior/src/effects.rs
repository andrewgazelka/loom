use crate::wire::{self, Descriptor, Mode, parse, value};
use loom_actor::{Ctx, RestartVerb, Rights, Trap, Value as SqlValue};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    cap: wire::Capability,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolve {
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnDriver {
    hash: String,
    init: Vec<u8>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscribe {
    cap: wire::Capability,
    table: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Unsubscribe {
    subscription_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    cap: wire::Capability,
    msg: Vec<u8>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stop {
    cap: wire::Capability,
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
    cap: wire::Capability,
    verb: RestartVerb,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendAfter {
    cap: wire::Capability,
    ms: u64,
    msg: Vec<u8>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Call {
    cap: wire::Capability,
    msg: Vec<u8>,
    timeout_ms: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    cap: wire::Capability,
    reference: String,
    msg: Vec<u8>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attenuate {
    cap: wire::Capability,
    rights: Rights,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapId {
    cap_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectSql {
    cap: wire::Capability,
    sql: String,
    params: Vec<wire::Cell>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Promote {
    cap: wire::Capability,
    behavior_hash: String,
    author: String,
    rationale: String,
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
        "actor.resolve" => {
            let request: Resolve = parse(&op, args)?;
            wire::capability_value(cx.resolve_name(&request.name).await?)
        }
        "actor.spawn_driver" => {
            let request: SpawnDriver = parse(&op, args)?;
            wire::capability_value(cx.spawn_driver(&request.hash, &request.init).await?)
        }
        "actor.sender_cap" => {
            parse::<()>(&op, args)?;
            wire::capability_value(cx.sender_cap().await?)
        }
        "actor.subscribe" => {
            let request: Subscribe = parse(&op, args)?;
            value(cx.subscribe(&request.cap.token, &request.table).await?)
        }
        "actor.unsubscribe" => {
            let request: Unsubscribe = parse(&op, args)?;
            cx.unsubscribe(&request.subscription_id).await?;
            Ok(Value::Null)
        }
        "sql" => {
            let request: wire::Sql = parse(&op, args)?;
            let params: Vec<SqlValue> = request.params.into_iter().map(Into::into).collect();
            let rows = cx.sql(&request.sql, params).await?;
            wire::rows(cx, rows)
        }
        "actor.accept" => {
            let request: Target = parse(&op, args)?;
            cx.accept(request.cap.token).await?;
            Ok(Value::Null)
        }
        "actor.attenuate" => {
            let request: Attenuate = parse(&op, args)?;
            wire::capability_value(cx.attenuate(&request.cap.token, request.rights).await?)
        }
        "actor.cap" => {
            let request: CapId = parse(&op, args)?;
            wire::capability_value(cx.cap(wire::cap_id(&op, &request.cap_id)?).await?)
        }
        "actor.revoke" => {
            let request: CapId = parse(&op, args)?;
            cx.revoke(wire::cap_id(&op, &request.cap_id)?).await?;
            Ok(Value::Null)
        }
        "actor.self_cap" => {
            parse::<()>(&op, args)?;
            wire::capability_value(cx.self_cap().await?)
        }
        "actor.promote" => {
            let request: Promote = parse(&op, args)?;
            cx.promote(
                &request.cap.token,
                &request.behavior_hash,
                &request.author,
                &request.rationale,
            )
            .await?;
            Ok(Value::Null)
        }
        "actor.inspect_sql" => {
            let request: InspectSql = parse(&op, args)?;
            let params = request.params.into_iter().map(Into::into).collect();
            wire::inspection(
                cx.inspect_sql(&request.cap.token, &request.sql, params)
                    .await?,
            )
        }
        "actor.send" => {
            let request: Send = parse(&op, args)?;
            cx.send(&request.cap.token, &request.msg).await?;
            Ok(Value::Null)
        }
        "actor.spawn" => {
            let mut spec = wire::child_spec(args)?;
            if spec.behavior_hash == "$self" {
                spec.behavior_hash = definition.into();
            }
            wire::capability_value(cx.spawn(&spec).await?)
        }
        "actor.stop" => {
            let request: Stop = parse(&op, args)?;
            cx.stop(&request.cap.token, &request.reason).await?;
            Ok(Value::Null)
        }
        "actor.monitor" => {
            let request: Target = parse(&op, args)?;
            value(cx.monitor(&request.cap.token).await?)
        }
        "actor.demonitor" => {
            let request: Demonitor = parse(&op, args)?;
            cx.demonitor(&request.reference, request.flush).await?;
            Ok(Value::Null)
        }
        "actor.link" => {
            let request: Target = parse(&op, args)?;
            cx.link(&request.cap.token).await?;
            Ok(Value::Null)
        }
        "actor.unlink" => {
            let request: Target = parse(&op, args)?;
            cx.unlink(&request.cap.token).await?;
            Ok(Value::Null)
        }
        "actor.shutdown" => {
            let request: Target = parse(&op, args)?;
            cx.shutdown(&request.cap.token).await?;
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
            cx.restart(&request.cap.token, request.verb).await?;
            Ok(Value::Null)
        }
        "actor.inspect" => {
            let request: Target = parse(&op, args)?;
            value(cx.inspect(&request.cap.token).await?)
        }
        "actor.send_after" => {
            let request: SendAfter = parse(&op, args)?;
            value(
                cx.send_after(&request.cap.token, request.ms, &request.msg)
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
                cx.call(&request.cap.token, &request.msg, request.timeout_ms)
                    .await?,
            )
        }
        "actor.reply" => {
            let request: Reply = parse(&op, args)?;
            cx.reply(&request.cap.token, &request.reference, &request.msg)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_requires_a_name_and_rejects_actor_ids() {
        let request: Resolve =
            parse("actor.resolve", serde_json::json!({"name":"service"})).unwrap();
        assert_eq!(request.name, "service");
        for args in [
            serde_json::json!({"target":"actor-id"}),
            serde_json::json!({"name":"service", "target":"actor-id"}),
            serde_json::json!({"name":42}),
        ] {
            assert!(parse::<Resolve>("actor.resolve", args).is_err());
        }
    }

    #[test]
    fn spawn_driver_requires_hash_and_byte_init() {
        let request: SpawnDriver = parse(
            "actor.spawn_driver",
            serde_json::json!({"hash":"driver-hash", "init":[0,255]}),
        )
        .unwrap();
        assert_eq!(request.hash, "driver-hash");
        assert_eq!(request.init, vec![0, 255]);
        for args in [
            serde_json::json!({"hash":"driver-hash"}),
            serde_json::json!({"hash":"driver-hash", "init":[256]}),
            serde_json::json!({"hash":"driver-hash", "init":[], "rights":255}),
        ] {
            assert!(parse::<SpawnDriver>("actor.spawn_driver", args).is_err());
        }
    }

    #[test]
    fn sender_cap_accepts_only_unit_arguments() {
        assert!(parse::<()>("actor.sender_cap", Value::Null).is_ok());
        assert!(parse::<()>("actor.sender_cap", serde_json::json!({"target":"other"})).is_err());
    }
}
