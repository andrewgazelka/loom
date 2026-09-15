use loom_sandbox::{CallEffects, Sandbox};
use loom_v8::{Limits, V8Engine};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin};

#[derive(Default)]
struct Effects {
    requests: Vec<Value>,
    reply: Value,
}
impl CallEffects for Effects {
    fn perform(
        &mut self,
        descriptor: Value,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + '_>> {
        self.requests.push(descriptor);
        Box::pin(async { Ok(self.reply.clone()) })
    }
}

#[tokio::test]
async fn json_unicode_receiver_and_private_bridges() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    let sandbox = engine.compile(r#"const main = loom.messages.json(value => ({value, roundtrip: loom.messages.decode(loom.messages.encode(value)), hidden: [typeof __loomEncode, typeof __loomDecode]}));"#).await.unwrap();
    let value = json!({"text":"雪😀 café","nested":[true,null,2]});
    let bytes = serde_json::to_vec(&value).unwrap();
    assert_eq!(
        sandbox
            .call(json!([bytes]), &mut Effects::default())
            .await
            .unwrap(),
        json!({"value":value,"roundtrip":value,"hidden":["undefined","undefined"]})
    );
}

#[tokio::test]
async fn actor_handles_copy_tokens_and_encode_wire_messages() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    let sandbox = engine.compile(r#"async function main() {
        const token = [1,2,255]; const peer = loom.actors.get(token); token[0]=99;
        await peer.send({text:'雪😀'}); await peer.sendBytes([0,255]);
        const ref = await peer.call(42,{timeoutMs:500});
        await peer.reply(ref, true); await peer.sendAfter(5,null); await peer.stop('normal');
        const accepted = await loom.actors.accept(peer.cap);
        const self = await loom.actors.self(); const child = await loom.actors.spawn({behavior_hash:'x'});
        return {peer,accepted,self,child,frozen:Object.isFrozen(peer)&&Object.isFrozen(peer.cap)&&Object.values(loom.actors).every(Object.isFrozen)};
    }"#).await.unwrap();
    let mut effects = Effects {
        reply: json!([4, 5]),
        ..Effects::default()
    };
    assert_eq!(
        sandbox.call(json!([]), &mut effects).await.unwrap(),
        json!({"peer":[1,2,255],"accepted":[1,2,255],"self":[4,5],"child":[4,5],"frozen":true})
    );
    let cap = json!([1, 2, 255]);
    assert_eq!(
        effects.requests,
        vec![
            json!({"op":"actor.send","args":{"cap":cap,"msg":serde_json::to_vec(&json!({"text":"雪😀"})).unwrap()}}),
            json!({"op":"actor.send","args":{"cap":cap,"msg":[0,255]}}),
            json!({"op":"actor.call","args":{"cap":cap,"msg":[52,50],"timeout_ms":500}}),
            json!({"op":"actor.reply","args":{"cap":cap,"reference":[4,5],"msg":[116,114,117,101]}}),
            json!({"op":"actor.send_after","args":{"cap":cap,"ms":5,"msg":[110,117,108,108]}}),
            json!({"op":"actor.stop","args":{"cap":cap,"reason":"normal"}}),
            json!({"op":"actor.accept","args":{"cap":cap}}),
            json!({"op":"actor.self_cap","args":null}),
            json!({"op":"actor.spawn","args":{"behavior_hash":"x"}}),
        ]
    );
}

#[tokio::test]
async fn malformed_messages_and_oversized_codecs_fail() {
    let engine = V8Engine::new(Limits {
        max_message_bytes: 128,
        ..Limits::default()
    })
    .unwrap();
    for expression in [
        "loom.messages.decode([256])",
        "loom.messages.decode([-1])",
        "loom.messages.decode([1.5])",
        "loom.messages.decode(['1'])",
        "loom.messages.decode([255])",
        "loom.messages.decode([123])",
        "loom.messages.decode(new Array(129).fill(32))",
        "loom.messages.encode('雪'.repeat(100))",
        "loom.messages.encode(undefined)",
        "loom.messages.encode(() => {})",
        "loom.messages.encode(Symbol())",
        "loom.messages.encode({toJSON() {return undefined;}})",
        "loom.messages.json(42)",
    ] {
        let sandbox = engine
            .compile(&format!("function main() {{ return {expression}; }}"))
            .await
            .unwrap();
        assert!(
            sandbox
                .call(json!([]), &mut Effects::default())
                .await
                .is_err(),
            "{expression}"
        );
    }
}

#[tokio::test]
async fn sql_uses_plain_parameters_and_named_rows() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    let sandbox=engine.compile("async function main() {return await loom.sql('SELECT ?', [null,42,1.5,'雪',loom.sql.blob([0,255])]);}").await.unwrap();
    let cells = json!([{"type":"null"},{"type":"integer","value":42},{"type":"real","value":1.5},{"type":"text","value":"雪"},{"type":"blob","value":[0,255]}]);
    let mut effects = Effects {
        reply: json!({"columns":["a","b","c","d","__proto__"],"rows":[cells]}),
        ..Effects::default()
    };
    assert_eq!(
        sandbox.call(json!([]), &mut effects).await.unwrap(),
        json!([{"a":null,"b":42,"c":1.5,"d":"雪","__proto__":[0,255]}])
    );
    assert_eq!(
        effects.requests,
        json!([{"op":"sql","args":{"sql":"SELECT ?","params":cells}}])
            .as_array()
            .unwrap()
            .clone()
    );
}

#[tokio::test]
async fn sql_rejects_ambiguous_results_and_unsupported_params() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    let sandbox = engine
        .compile("async function main() {return await loom.sql('SELECT 1');}")
        .await
        .unwrap();
    for reply in [
        json!({"columns":["x","x"],"rows":[]}),
        json!({"columns":["x"],"rows":[[{"type":"mystery"}]]}),
        json!({"columns":["x"],"rows":[[]]}),
    ] {
        assert!(
            sandbox
                .call(
                    json!([]),
                    &mut Effects {
                        reply,
                        ..Effects::default()
                    }
                )
                .await
                .is_err()
        );
    }
    for param in ["[1]", "true", "Infinity", "9007199254740992"] {
        let sandbox = engine
            .compile(&format!(
                "async function main() {{return await loom.sql('SELECT ?', [{param}]);}}"
            ))
            .await
            .unwrap();
        let mut effects = Effects::default();
        assert!(sandbox.call(json!([]), &mut effects).await.is_err());
        assert!(effects.requests.is_empty());
    }
}
