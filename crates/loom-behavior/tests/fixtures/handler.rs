use loom::serde_json::{Value, json};

#[loom::schema]
pub fn schema() -> &'static str {
    "CREATE TABLE entries(body BLOB NOT NULL)"
}

#[loom::def]
pub fn handle(msg: Vec<u8>) {
    let request: Value = loom::serde_json::from_slice(&msg).unwrap();
    match request["action"].as_str().unwrap() {
        "init" => {}
        "row" => {
            if request["scoped"].as_bool() == Some(true) {
                loom::handle(
                    ["local"],
                    |effect, _| {
                        assert_eq!(effect.name, "local");
                        loom::Reply::Resume(json!(17))
                    },
                    || {
                        loom::scope(|scope| {
                            let child = scope
                                .spawn(|| {
                                    let result: i64 = loom::perform("local", json!({})).unwrap();
                                    assert_eq!(result, 17);
                                    insert(&msg);
                                })
                                .unwrap();
                            child.join().unwrap();
                        })
                    },
                )
                .unwrap();
            } else {
                insert(&msg);
            }
        }
        "send" => {
            let cap: loom::actor::Cap =
                loom::serde_json::from_value(request["cap"].clone()).unwrap();
            let cap = loom::actor::accept(cap).unwrap();
            let token: Value = loom::serde_json::from_slice(&cap.token).unwrap();
            let cap_id = token["cap_id"].as_u64().unwrap().to_string();
            let restored: Vec<u8> = loom::perform("actor.cap", json!({"cap_id":cap_id})).unwrap();
            assert_eq!(restored, cap.token);
            let body = loom::serde_json::to_vec(&json!({"action":"row","forwarded":true})).unwrap();
            loom::actor::send(&cap, &body).unwrap();
            assert!(request["trap"].as_bool() != Some(true), "trap after send");
        }
        "forged_send" => {
            let body = loom::serde_json::to_vec(&json!({"action":"row"})).unwrap();
            // Ignoring a capability rejection must still abort the transaction.
            let _ = loom::perform::<()>("actor.send", json!({"cap":request["cap"],"msg":body}));
        }
        "spawn_send" => {
            let init = loom::serde_json::to_vec(&json!({"action":"init"})).unwrap();
            let cap: Vec<u8> = loom::perform(
                "actor.spawn",
                json!({
                    "behavior_hash":request["behavior_hash"], "init":init
                }),
            )
            .unwrap();
            let body = loom::serde_json::to_vec(&json!({"action":"row"})).unwrap();
            loom::perform::<()>("actor.send", json!({"cap":cap,"msg":body})).unwrap();
            let token: Value = loom::serde_json::from_slice(&cap).unwrap();
            let cap_id = token["cap_id"].as_u64().unwrap().to_string();
            loom::perform::<()>("actor.revoke", json!({"cap_id":cap_id})).unwrap();
        }
        "unknown" => {
            // Swallowing a rejected root effect must not make a message commit.
            let _ = loom::perform::<Value>("nope", json!({"request":[]}));
        }
        "unknown_actor" => {
            let _ = loom::perform::<Value>("actor.nope", json!({"request":[]}));
        }
        "effect" => {
            insert(&msg);
            let _ = loom::perform::<Vec<u8>>("test.effect", json!({"request":[1,2,3]}));
        }
        "scoped_trap" => {
            loom::scope(|scope| {
                scope
                    .spawn(|| {
                        loom::perform::<Vec<u8>>("test.effect", json!({"request":[1,2,3]}))
                            .unwrap();
                        panic!("scoped fixture panic");
                    })
                    .unwrap()
                    .join()
                    .unwrap();
            });
        }
        "handler_trap" => {
            loom::perform::<Vec<u8>>("test.effect", json!({"request":[1,2,3]})).unwrap();
            let _ = loom::handle(
                ["local"],
                |_, _| panic!("handler fixture panic"),
                || loom::perform::<Value>("local", Value::Null),
            );
        }
        action => panic!("unknown fixture action {action}"),
    }
}

fn insert(msg: &[u8]) {
    let rows: Value = loom::perform(
        "sql",
        json!({
            "sql":"INSERT INTO entries(body) VALUES (?) RETURNING body",
            "params":[{"type":"blob","value":msg}]
        }),
    )
    .unwrap();
    assert_eq!(rows["columns"], json!(["body"]));
    assert_eq!(rows["rows"], json!([[{"type":"blob","value":msg}]]));
}
