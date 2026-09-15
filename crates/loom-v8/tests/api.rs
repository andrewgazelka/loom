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
        const self = await loom.actors.self(); const child = await loom.actors.spawn({behavior:'x', init:{hello:'雪'}});
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
            json!({"op":"actor.spawn","args":{"behavior_hash":"x", "init":serde_json::to_vec(&json!({"hello":"雪"})).unwrap()}}),
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

#[tokio::test]
async fn native_resource_handles_use_actor_capabilities_and_json_commands() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    let sandbox = engine.compile(r#"async function main() {
        const peer = await loom.actors.named('worker');
        const sender = await loom.actors.sender();
        const process = await loom.processes.spawn('claude', {subscriber:peer});
        await process.write('雪\n'); await process.closeStdin();
        await process.subscribe(sender.cap); await process.cancel();
        const namedProcess = await loom.processes.named('running-worker');
        await namedProcess.write('next');
        const listener = await loom.websockets.listen({maxConnections:8,maxMessageBytes:4096});
        const socket = await loom.websockets.sender();
        await socket.send('雪😀'); await socket.sendBytes([0,255]); await socket.close();
        const restoredProcess = loom.processes.get(JSON.parse(JSON.stringify(process)));
        const restoredSocket = loom.websockets.get(JSON.parse(JSON.stringify(socket)));
        return {listener,process,socket,restoredProcess,restoredSocket,
            frozen:Object.isFrozen(process)&&Object.isFrozen(socket)&&Object.isFrozen(loom.processes)&&Object.isFrozen(loom.websockets)};
    }"#).await.unwrap();
    let cap = json!([4, 5]);
    let mut effects = Effects {
        reply: cap.clone(),
        ..Effects::default()
    };
    assert_eq!(
        sandbox.call(json!([]), &mut effects).await.unwrap(),
        json!({"listener":cap,"process":cap,"socket":cap,"restoredProcess":cap,"restoredSocket":cap,"frozen":true})
    );
    assert_eq!(
        effects.requests[0],
        json!({"op":"actor.resolve","args":{"name":"worker"}})
    );
    assert_eq!(
        effects.requests[1],
        json!({"op":"actor.sender_cap","args":null})
    );
    assert_eq!(effects.requests[2]["op"], "actor.spawn");
    assert_eq!(effects.requests[2]["args"]["behavior_hash"], "process-v1");
    assert_eq!(
        decode_bytes(&effects.requests[2]["args"]["init"]),
        json!({"process":"claude","subscriber":cap})
    );
    let expected = [
        json!({"type":"stdin","data":"雪\n"}),
        json!({"type":"close_stdin"}),
        json!({"type":"subscribe","cap":cap}),
        json!({"type":"cancel"}),
    ];
    for (index, command) in expected.into_iter().enumerate() {
        let request = &effects.requests[index + 3];
        assert_eq!(request["op"], "actor.send");
        assert_eq!(request["args"]["cap"], cap);
        assert_eq!(decode_bytes(&request["args"]["msg"]), command);
    }
    assert_eq!(
        effects.requests[7],
        json!({"op":"actor.resolve","args":{"name":"running-worker"}})
    );
    assert_eq!(
        decode_bytes(&effects.requests[8]["args"]["msg"]),
        json!({"type":"stdin","data":"next"})
    );
    assert_eq!(
        effects.requests[9],
        json!({"op":"actor.spawn_driver","args":{"hash":"websocket-v1","init":b"{\"max_connections\":8,\"max_message_bytes\":4096}".to_vec()}})
    );
    assert_eq!(
        effects.requests[10],
        json!({"op":"actor.sender_cap","args":null})
    );
    for (index, command) in [
        json!({"type":"send","data":{"type":"text","text":"雪😀"}}),
        json!({"type":"send","data":{"type":"binary","bytes":[0,255]}}),
        json!({"type":"close","code":1000,"reason":""}),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            decode_bytes(&effects.requests[index + 11]["args"]["msg"]),
            command
        );
    }
}

fn decode_bytes(value: &Value) -> Value {
    let bytes: Vec<u8> = serde_json::from_value(value.clone()).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn resource_helpers_reject_ambient_authority_options_and_old_spawn_schema() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    for expression in [
        "loom.actors.spawn({behavior_hash:'old',init:[]})",
        "loom.actors.spawn({behavior:'worker',tenant:'other'})",
        "loom.processes.spawn('claude',{program:'/bin/sh'})",
        "loom.processes.spawn('claude',{env:{SECRET:'x'}})",
        "loom.actors.named({tenant:'other',name:'worker'})",
        "loom.websockets.listen({tenant:'other'})",
        "loom.websockets.listen({maxConnections:4097})",
        "loom.websockets.listen({maxMessageBytes:0})",
        "loom.websockets.get([1]).send([1,2])",
        "loom.websockets.get([1]).sendBytes([256])",
        "loom.processes.get([1]).write([1,2])",
    ] {
        let sandbox = engine
            .compile(&format!(
                "async function main() {{return await {expression};}}"
            ))
            .await
            .unwrap();
        let mut effects = Effects::default();
        assert!(
            sandbox.call(json!([]), &mut effects).await.is_err(),
            "{expression}"
        );
        assert!(effects.requests.is_empty(), "{expression}");
    }
}

#[tokio::test]
async fn actor_message_limit_counts_original_bytes_and_json_avoids_byte_expansion() {
    let engine = V8Engine::new(Limits {
        max_message_bytes: 128,
        ..Limits::default()
    })
    .unwrap();
    assert_eq!(engine.max_message_bytes(), 128);
    let raw=engine.compile("function main(bytes) {return {length:bytes.length,first:bytes[0],last:bytes[bytes.length-1]};}").await.unwrap();
    let message = vec![255; 128];
    assert_eq!(
        raw.call_message(&message, &mut Effects::default())
            .await
            .unwrap(),
        json!({"length":128,"first":255,"last":255})
    );
    assert!(
        raw.call(json!([message]), &mut Effects::default())
            .await
            .is_err()
    );
    assert!(
        raw.call_message(&vec![255; 129], &mut Effects::default())
            .await
            .is_err()
    );
    let receiver=engine.compile("const main=loom.messages.json(value=>({length:value.text.length,hidden:typeof __loomJson}));").await.unwrap();
    let encoded = serde_json::to_vec(&json!({"text":"x".repeat(117)})).unwrap();
    assert_eq!(encoded.len(), 128);
    assert_eq!(
        receiver
            .call_message(&encoded, &mut Effects::default())
            .await
            .unwrap(),
        json!({"length":117,"hidden":"undefined"})
    );
    for message in [vec![255], b"{".to_vec(), b"9007199254740993".to_vec()] {
        assert!(
            receiver
                .call_message(&message, &mut Effects::default())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn json_wrapper_marker_is_private_and_positional_calls_keep_byte_semantics() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    let wrapper = engine
        .compile(r#"const main=loom.messages.json(value=>value.text);"#)
        .await
        .unwrap();
    let message = br#"{"text":"hello"}"#;
    assert_eq!(
        wrapper
            .call_message(message, &mut Effects::default())
            .await
            .unwrap(),
        json!("hello")
    );
    assert_eq!(
        wrapper
            .call(json!([message]), &mut Effects::default())
            .await
            .unwrap(),
        json!("hello")
    );
    let caught = engine
        .compile(
            r#"function main() {
        const handler = loom.messages.json(() => {throw new Error('handled');});
        try {handler([110,117,108,108]);} catch (error) {return error.message;}
    }"#,
        )
        .await
        .unwrap();
    assert_eq!(
        caught
            .call(json!([]), &mut Effects::default())
            .await
            .unwrap(),
        json!("handled")
    );
    let forged = engine
        .compile(
            r#"function main(value) {return Array.isArray(value);}
        main['loom.messages.json#handler']=value=>false;
        main[Symbol.for('loom.messages.json#handler')]=value=>false;"#,
        )
        .await
        .unwrap();
    assert_eq!(
        forged
            .call_message(message, &mut Effects::default())
            .await
            .unwrap(),
        json!(true)
    );
}

#[tokio::test]
async fn actor_effect_budget_counts_payload_and_metadata_separately() {
    let engine = V8Engine::new(Limits {
        max_message_bytes: 128,
        ..Limits::default()
    })
    .unwrap();
    let sender=engine.compile("async function main(n) { await loom.actors.get([1]).sendBytes(new Array(n).fill(255)); }").await.unwrap();
    let mut effects = Effects::default();
    sender.call(json!([128]), &mut effects).await.unwrap();
    assert_eq!(effects.requests[0]["args"]["msg"], json!(vec![255; 128]));
    let mut refused = Effects::default();
    assert!(sender.call(json!([129]), &mut refused).await.is_err());
    assert!(refused.requests.is_empty());
    let bad_metadata=engine.compile("async function main() {await loom.perform('actor.send',{cap:[1],msg:[1],extra:'x'.repeat(128)});}").await.unwrap();
    assert!(bad_metadata.call(json!([]), &mut refused).await.is_err());
    assert!(refused.requests.is_empty());
    let bad_byte = engine
        .compile("async function main() {await loom.perform('actor.send',{cap:[1],msg:[256]});}")
        .await
        .unwrap();
    assert!(bad_byte.call(json!([]), &mut refused).await.is_err());
    assert!(refused.requests.is_empty());
}

#[tokio::test]
async fn large_json_actor_echo_crosses_native_input_and_effect_boundaries() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    let receiver=engine.compile("const main=loom.messages.json(async value => {await loom.actors.get([1]).send(value);});").await.unwrap();
    let value = json!({"text":"雪".repeat(100_000)});
    let message = serde_json::to_vec(&value).unwrap();
    let mut effects = Effects::default();
    receiver.call_message(&message, &mut effects).await.unwrap();
    assert_eq!(decode_bytes(&effects.requests[0]["args"]["msg"]), value);
}

#[tokio::test]
async fn container_spawn_maps_limits_and_reuses_process_messages() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    let sandbox = engine
        .compile(
            r#"async function main() {
        const process=await loom.containers.spawn({
            image:'alpine:3.21',network:'none',command:'/bin/cat',args:[],env:{LANG:'C.UTF-8'},
            limits:{memoryMb:64,cpus:0.5,pids:16},ttlMs:60000,subscriber:loom.actors.get([9]),
        });
        await process.write('雪\n'); await process.closeStdin();
        await process.subscribe([8]); await process.cancel();
        return {process,frozen:Object.isFrozen(process)&&Object.isFrozen(loom.containers)};
    }"#,
        )
        .await
        .unwrap();
    let mut effects = Effects {
        reply: json!([4, 5]),
        ..Effects::default()
    };
    assert_eq!(
        sandbox.call(json!([]), &mut effects).await.unwrap(),
        json!({"process":[4,5],"frozen":true})
    );
    assert_eq!(effects.requests[0]["op"], "actor.spawn");
    assert_eq!(effects.requests[0]["args"]["behavior_hash"], "container-v1");
    assert_eq!(
        decode_bytes(&effects.requests[0]["args"]["init"]),
        json!({"container":{"image":"alpine:3.21","network":"none","command":"/bin/cat","args":[],"env":{"LANG":"C.UTF-8"},"memoryMb":64,"cpus":0.5,"pids":16,"ttlMs":60000},"subscriber":[9]})
    );
    for (index, command) in [
        json!({"type":"stdin","data":"雪\n"}),
        json!({"type":"close_stdin"}),
        json!({"type":"subscribe","cap":[8]}),
        json!({"type":"cancel"}),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(effects.requests[index + 1]["op"], "actor.send");
        assert_eq!(effects.requests[index + 1]["args"]["cap"], json!([4, 5]));
        assert_eq!(
            decode_bytes(&effects.requests[index + 1]["args"]["msg"]),
            command
        );
    }
}

#[tokio::test]
async fn container_defaults_stay_host_owned_and_host_escape_fields_are_rejected() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    let defaults = engine
        .compile(
            "async function main() {return await loom.containers.spawn({image:'alpine:3.21'});}",
        )
        .await
        .unwrap();
    let mut effects = Effects {
        reply: json!([4, 5]),
        ..Effects::default()
    };
    defaults.call(json!([]), &mut effects).await.unwrap();
    assert_eq!(
        decode_bytes(&effects.requests[0]["args"]["init"]),
        json!({"container":{"image":"alpine:3.21"}})
    );
    let bridge = engine.compile("async function main() {return await loom.containers.spawn({image:'alpine:3.21',network:'bridge'});}").await.unwrap();
    let mut bridge_effects = Effects {
        reply: json!([4, 5]),
        ..Effects::default()
    };
    bridge.call(json!([]), &mut bridge_effects).await.unwrap();
    assert_eq!(
        decode_bytes(&bridge_effects.requests[0]["args"]["init"]),
        json!({"container":{"image":"alpine:3.21","network":"bridge"}})
    );
    for field in [
        "socket:'/var/run/docker.sock'",
        "privileged:true",
        "mounts:['/:/host']",
        "network:'host'",
        "tenant:'other'",
        "flags:['--privileged']",
        "limits:{cpus:Infinity}",
        "limits:{memoryMb:0}",
        "limits:{pids:1.5}",
        "ttlMs:0",
        "env:{A:42}",
        "args:'--help'",
    ] {
        let source = format!(
            "async function main() {{return await loom.containers.spawn({{image:'alpine:3.21',{field}}});}}"
        );
        let sandbox = engine.compile(&source).await.unwrap();
        let mut refused = Effects::default();
        assert!(
            sandbox.call(json!([]), &mut refused).await.is_err(),
            "{field}"
        );
        assert!(refused.requests.is_empty(), "{field}");
    }
}

#[tokio::test]
async fn lifecycle_callbacks_are_host_selected_and_have_fresh_isolates() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    let actor=engine.compile(r#"let count=0;
        const main=loom.actor({
            onStart: async()=>{await loom.perform('phase',{kind:'start',count:++count});},
            onMessage: async message=>{await loom.perform('phase',{kind:'message',message,count:++count,hidden:typeof __loomActor});},
            onStop: async reason=>{await loom.perform('phase',{kind:'stop',reason,count:++count});},
        });"#).await.unwrap();
    assert!(actor.has_startup());
    assert!(actor.has_shutdown());
    let mut effects = Effects::default();
    assert_eq!(actor.call_startup(&mut effects).await.unwrap(), Value::Null);
    actor
        .call_message(br#"{"type":"onStart"}"#, &mut effects)
        .await
        .unwrap();
    actor
        .call_message(br#"{"type":"shutdown"}"#, &mut effects)
        .await
        .unwrap();
    assert_eq!(
        actor
            .call_shutdown("node_shutdown", &mut effects)
            .await
            .unwrap(),
        Value::Null
    );
    actor.call_startup(&mut effects).await.unwrap();
    assert_eq!(
        effects.requests,
        vec![
            json!({"op":"phase","args":{"kind":"start","count":1}}),
            json!({"op":"phase","args":{"kind":"message","message":{"type":"onStart"},"count":1,"hidden":"undefined"}}),
            json!({"op":"phase","args":{"kind":"message","message":{"type":"shutdown"},"count":1,"hidden":"undefined"}}),
            json!({"op":"phase","args":{"kind":"stop","reason":"node_shutdown","count":1}}),
            json!({"op":"phase","args":{"kind":"start","count":1}}),
        ]
    );
}

#[tokio::test]
async fn absent_and_forged_lifecycle_hooks_do_not_run_message_handlers() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    for source in [
        "const main=loom.actor({onMessage:()=>loom.perform('unexpected',null)});",
        "function main() {return loom.perform('unexpected',null);} main['loom.actor#startup']=main; main[Symbol.for('loom.actor#shutdown')]=main;",
        "const main=loom.messages.json(()=>loom.perform('unexpected',null));",
    ] {
        let sandbox = engine.compile(source).await.unwrap();
        assert!(!sandbox.has_startup());
        assert!(!sandbox.has_shutdown());
        let mut effects = Effects::default();
        assert_eq!(
            sandbox.call_startup(&mut effects).await.unwrap(),
            Value::Null
        );
        assert_eq!(
            sandbox
                .call_shutdown("node_shutdown", &mut effects)
                .await
                .unwrap(),
            Value::Null
        );
        assert!(effects.requests.is_empty());
    }
    for handlers in [
        "{}",
        "{onMessage:42}",
        "{onMessage(){},onStart:null}",
        "{onMessage(){},onStop:42}",
        "{onMessage(){},startup(){}}",
    ] {
        assert!(
            engine
                .compile(&format!("const main=loom.actor({handlers});"))
                .await
                .is_err(),
            "{handlers}"
        );
    }
}

struct LifecycleFailure;
impl CallEffects for LifecycleFailure {
    fn perform(
        &mut self,
        _: Value,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + '_>> {
        Box::pin(async { Err(std::io::Error::other("lifecycle storage unavailable").into()) })
    }
}

#[tokio::test]
async fn lifecycle_host_failures_keep_type_and_shutdown_reasons_are_bounded() {
    let engine = V8Engine::new(Limits {
        max_message_bytes: 128,
        ..Limits::default()
    })
    .unwrap();
    let sandbox = engine
        .compile(
            r#"const main=loom.actor({
        async onStart(){try{await loom.perform('state',null);}catch{}},
        onMessage(){},
        async onStop(reason){try{await loom.perform('state',reason);}catch{}},
    });"#,
        )
        .await
        .unwrap();
    let error = sandbox
        .call_startup(&mut LifecycleFailure)
        .await
        .unwrap_err();
    assert!(error.is::<std::io::Error>(), "{error:#}");
    let error = sandbox
        .call_shutdown("node_shutdown", &mut LifecycleFailure)
        .await
        .unwrap_err();
    assert!(error.is::<std::io::Error>(), "{error:#}");
    let mut effects = Effects::default();
    assert!(
        sandbox
            .call_shutdown(&"x".repeat(129), &mut effects)
            .await
            .is_err()
    );
    assert!(effects.requests.is_empty());
}
