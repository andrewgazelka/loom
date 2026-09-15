use loom_sandbox::{CallEffects, Sandbox};
use loom_v8::{Limits, V8Engine};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin};

// The host owns CID parsing. This fixture checks that JS keeps an opaque
// reference unchanged across the effect boundary and fresh isolates.
const REFERENCE: &str = "bafkreigh2akiscaildcvfsw4jluytvgnvciyq6kydl4vwotjyq4lg7l6mi";

#[derive(Default)]
struct Effects {
    requests: Vec<Value>,
    reply: Value,
}
impl CallEffects for Effects {
    fn perform(
        &mut self,
        request: Value,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + '_>> {
        self.requests.push(request);
        Box::pin(async { Ok(self.reply.clone()) })
    }
}

#[tokio::test]
async fn cas_raw_and_json_operations_preserve_the_canonical_wire() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    let put=engine.compile("async function main(value) {const ref=await loom.cas.put(value); return {ref,frozen:Object.isFrozen(ref)&&Object.isFrozen(loom.cas)};}").await.unwrap();
    let reference = json!({"$ref":REFERENCE});
    let mut effects = Effects {
        reply: reference.clone(),
        ..Effects::default()
    };
    assert_eq!(
        put.call(json!([[0, 128, 255]]), &mut effects)
            .await
            .unwrap(),
        json!({"ref":reference,"frozen":true})
    );
    assert_eq!(
        effects.requests,
        vec![json!({"op":"cas.put_bytes","args":{"bytes":[0,128,255]}})]
    );

    let get = engine
        .compile("async function main(reference) {return await loom.cas.get(reference);}")
        .await
        .unwrap();
    let mut effects = Effects {
        reply: json!([0, 128, 255]),
        ..Effects::default()
    };
    assert_eq!(
        get.call(json!([reference]), &mut effects).await.unwrap(),
        json!([0, 128, 255])
    );
    assert_eq!(
        effects.requests,
        vec![json!({"op":"cas.get_bytes","args":{"reference":reference}})]
    );

    let put_json = engine
        .compile("async function main(value) {return await loom.cas.putJson(value);}")
        .await
        .unwrap();
    let value = json!({"text":"雪😀","child":reference});
    let mut effects = Effects {
        reply: reference.clone(),
        ..Effects::default()
    };
    assert_eq!(
        put_json.call(json!([value]), &mut effects).await.unwrap(),
        reference
    );
    assert_eq!(effects.requests, vec![json!({"op":"cas.put","args":value})]);

    let get_json = engine
        .compile("async function main(reference) {return await loom.cas.getJson(reference);}")
        .await
        .unwrap();
    let mut effects = Effects {
        reply: value.clone(),
        ..Effects::default()
    };
    assert_eq!(
        get_json
            .call(json!([reference]), &mut effects)
            .await
            .unwrap(),
        value
    );
    assert_eq!(
        effects.requests,
        vec![json!({"op":"cas.get","args":{"hash":REFERENCE}})]
    );
}

#[tokio::test]
async fn cas_references_pass_through_actor_messages_and_fresh_isolates() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    let sender=engine.compile("const main=loom.messages.json(async ref=>{await loom.actors.get([1]).send({artifact:ref});});").await.unwrap();
    let reference = json!({"$ref":REFERENCE});
    let mut effects = Effects::default();
    sender
        .call_message(&serde_json::to_vec(&reference).unwrap(), &mut effects)
        .await
        .unwrap();
    let message: Vec<u8> =
        serde_json::from_value(effects.requests[0]["args"]["msg"].clone()).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&message).unwrap(),
        json!({"artifact":reference})
    );
    let receiver = engine
        .compile(
            "const main=loom.messages.json(async message=>await loom.cas.get(message.artifact));",
        )
        .await
        .unwrap();
    let mut effects = Effects {
        reply: json!([42]),
        ..Effects::default()
    };
    assert_eq!(
        receiver.call_message(&message, &mut effects).await.unwrap(),
        json!([42])
    );
    assert_eq!(
        effects.requests,
        vec![json!({"op":"cas.get_bytes","args":{"reference":reference}})]
    );
}

#[tokio::test]
async fn cas_rejects_bad_bytes_references_and_host_result_shapes() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    for expression in [
        "loom.cas.put('not bytes')",
        "loom.cas.put([256])",
        "loom.cas.put([-1])",
        "loom.cas.put([1.5])",
        "loom.cas.get('a cid')",
        "loom.cas.get({$ref:''})",
        "loom.cas.get({$ref:42})",
        "loom.cas.get({$ref:'cid',path:'/etc/passwd'})",
        "loom.cas.getJson({hash:'cid'})",
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
    let put = engine
        .compile("async function main() {return await loom.cas.put([1]);}")
        .await
        .unwrap();
    for reply in [
        json!("cid"),
        json!({"$ref":""}),
        json!({"$ref":REFERENCE,"extra":true}),
    ] {
        assert!(
            put.call(
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
    let get = engine
        .compile("async function main(ref) {return await loom.cas.get(ref);}")
        .await
        .unwrap();
    assert!(
        get.call(
            json!([{"$ref":REFERENCE}]),
            &mut Effects {
                reply: json!([256]),
                ..Effects::default()
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn cas_effects_obey_the_configured_v8_message_budget() {
    let engine = V8Engine::new(Limits {
        max_message_bytes: 128,
        ..Limits::default()
    })
    .unwrap();
    let put = engine
        .compile("async function main() {return await loom.cas.put(new Array(129).fill(255));}")
        .await
        .unwrap();
    let mut effects = Effects::default();
    assert!(put.call(json!([]), &mut effects).await.is_err());
    assert!(effects.requests.is_empty());
    let get = engine
        .compile("async function main(ref) {return await loom.cas.get(ref);}")
        .await
        .unwrap();
    let mut effects = Effects {
        reply: json!(vec![255; 129]),
        ..Effects::default()
    };
    assert!(
        get.call(json!([{"$ref":REFERENCE}]), &mut effects)
            .await
            .is_err()
    );
    assert_eq!(effects.requests.len(), 1);
}

#[tokio::test]
async fn vm_spawn_uses_a_cas_image_and_the_shared_process_interface() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    let sandbox=engine.compile(r#"async function main(image) {
        const vm=await loom.vms.spawn({image,command:'/bin/sh',args:['-i'],env:{LANG:'C.UTF-8'},cwd:'/',network:'none',limits:{memoryMb:256,cpus:2,rootfsMb:64},ttlMs:30000,subscriber:loom.actors.get([9])});
        await vm.write('echo 雪\n'); await vm.closeStdin(); await vm.subscribe([8]); await vm.cancel();
        return {vm,frozen:Object.isFrozen(vm)&&Object.isFrozen(loom.vms)};
    }"#).await.unwrap();
    let image = json!({"$ref":REFERENCE});
    let mut effects = Effects {
        reply: json!([4, 5]),
        ..Effects::default()
    };
    assert_eq!(
        sandbox.call(json!([image]), &mut effects).await.unwrap(),
        json!({"vm":[4,5],"frozen":true})
    );
    assert_eq!(effects.requests[0]["op"], "actor.spawn");
    assert_eq!(effects.requests[0]["args"]["behavior_hash"], "vm-v1");
    let init: Vec<u8> =
        serde_json::from_value(effects.requests[0]["args"]["init"].clone()).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&init).unwrap(),
        json!({"vm":{"image":image,"command":"/bin/sh","args":["-i"],"env":{"LANG":"C.UTF-8"},"cwd":"/","network":"none","memoryMb":256,"cpus":2,"rootfsMb":64,"ttlMs":30000},"subscriber":[9]})
    );
    for (index, command) in [
        json!({"type":"stdin","data":"echo 雪\n"}),
        json!({"type":"close_stdin"}),
        json!({"type":"subscribe","cap":[8]}),
        json!({"type":"cancel"}),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(effects.requests[index + 1]["op"], "actor.send");
        assert_eq!(effects.requests[index + 1]["args"]["cap"], json!([4, 5]));
        let bytes: Vec<u8> =
            serde_json::from_value(effects.requests[index + 1]["args"]["msg"].clone()).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), command);
    }
}

#[tokio::test]
async fn vm_helpers_reject_host_paths_devices_and_invalid_guest_options() {
    let engine = V8Engine::new(Limits::default()).unwrap();
    for field in [
        "image:'/host/rootfs'",
        "image:{$ref:'cid',path:'/host/rootfs'}",
        "command:'sh'",
        "cwd:'relative'",
        "host:'/host'",
        "mounts:['/:/host']",
        "kernel:'/host/vmlinuz'",
        "devices:['/dev/kvm']",
        "vsock:1234",
        "tenant:'other'",
        "network:'host'",
        "network:'bridge'",
        "limits:{cpus:0.5}",
        "limits:{memoryMb:0}",
        "limits:{rootfsMb:-1}",
        "limits:{pids:8}",
        "ttlMs:0",
        "args:'-i'",
        "env:{LANG:42}",
    ] {
        let source = format!(
            "async function main() {{return await loom.vms.spawn({{image:{{$ref:'{REFERENCE}'}},command:'/bin/sh',{field}}});}}"
        );
        let sandbox = engine.compile(&source).await.unwrap();
        let mut effects = Effects::default();
        assert!(
            sandbox.call(json!([]), &mut effects).await.is_err(),
            "{field}"
        );
        assert!(effects.requests.is_empty(), "{field}");
    }
}

#[tokio::test]
async fn cas_guest_small_blob_ceiling_fits_native_transport_in_both_directions() {
    // Matches the backend's CAS_GUEST_MAX_BYTES contract without introducing a
    // V8->store dependency. Backend tests enforce the actual admission ceiling.
    let byte_count = 128 * 1024;
    let engine = V8Engine::new(Limits::default()).unwrap();
    let put = engine
        .compile("async function main(n) {return await loom.cas.put(new Array(n).fill(255));}")
        .await
        .unwrap();
    let reference = json!({"$ref":REFERENCE});
    let mut effects = Effects {
        reply: reference.clone(),
        ..Effects::default()
    };
    assert_eq!(
        put.call(json!([byte_count]), &mut effects).await.unwrap(),
        reference
    );
    assert_eq!(
        effects.requests[0]["args"]["bytes"]
            .as_array()
            .unwrap()
            .len(),
        byte_count
    );
    let get = engine
        .compile("async function main(ref) {return await loom.cas.get(ref);}")
        .await
        .unwrap();
    let mut effects = Effects {
        reply: json!(vec![255; byte_count]),
        ..Effects::default()
    };
    let returned = get.call(json!([reference]), &mut effects).await.unwrap();
    assert_eq!(returned.as_array().unwrap().len(), byte_count);
    assert!(
        returned
            .as_array()
            .unwrap()
            .iter()
            .all(|value| value == 255)
    );
}
