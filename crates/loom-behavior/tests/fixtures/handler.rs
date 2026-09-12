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
            let body = loom::serde_json::to_vec(&json!({"action":"row","forwarded":true})).unwrap();
            loom::perform::<()>("actor.send", json!({"target":request["target"],"msg":body}))
                .unwrap();
            assert!(request["trap"].as_bool() != Some(true), "trap after send");
        }
        "unknown" => {
            // Swallowing a rejected root effect must not make a message commit.
            let _ = loom::perform::<Value>("nope", json!({"request":[]}));
        }
        "effect" => {
            insert(&msg);
            let _ = loom::perform::<Vec<u8>>("test.effect", json!({"request":[1,2,3]}));
        }
        "scoped_trap" => {
            loom::scope(|scope| {
                scope.spawn(|| {
                    loom::perform::<Vec<u8>>("test.effect", json!({"request":[1,2,3]})).unwrap();
                    panic!("scoped fixture panic");
                }).unwrap().join().unwrap();
            });
        }
        "handler_trap" => {
            loom::perform::<Vec<u8>>("test.effect", json!({"request":[1,2,3]})).unwrap();
            let _ = loom::handle(["local"], |_, _| panic!("handler fixture panic"), || {
                loom::perform::<Value>("local", Value::Null)
            });
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
