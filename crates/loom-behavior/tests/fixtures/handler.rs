use loom::serde_json::Value;

pub const LOOM_SCHEMA: &str = "CREATE TABLE entries(body BLOB NOT NULL)";

pub fn handle(msg: Vec<u8>) {
    let request: Value = loom::serde_json::from_slice(&msg).unwrap();
    match request["action"].as_str().unwrap() {
        "init" => {}
        "row" => {
            if request["scoped"].as_bool() == Some(true) {
                loom::handle(
                    ["local"],
                    |effect, _| {
                        check(effect.name == "local");
                        loom::Reply::Resume(Value::from(17))
                    },
                    || {
                        loom::scope(|scope| {
                            let child = scope
                                .spawn(|| {
                                    let result: i64 = loom::perform("local", object([])).unwrap();
                                    check(result == 17);
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
            let restored: Vec<u8> =
                loom::perform("actor.cap", object([field("cap_id", Value::from(cap_id))])).unwrap();
            check(restored == cap.token);
            let body = loom::serde_json::to_vec(&object([
                field("action", Value::from("row")),
                field("forwarded", Value::from(true)),
            ]))
            .unwrap();
            loom::actor::send(&cap, &body).unwrap();
            check(request["trap"].as_bool() != Some(true));
        }
        "forged_send" => {
            let body =
                loom::serde_json::to_vec(&object([field("action", Value::from("row"))])).unwrap();
            // Ignoring a capability rejection must still abort the transaction.
            let _ = loom::perform::<()>(
                "actor.send",
                object([
                    field("cap", request["cap"].clone()),
                    field("msg", bytes(&body)),
                ]),
            );
        }
        "spawn_send" => {
            let init =
                loom::serde_json::to_vec(&object([field("action", Value::from("init"))])).unwrap();
            let cap: Vec<u8> = loom::perform(
                "actor.spawn",
                object([
                    field("behavior_hash", request["behavior_hash"].clone()),
                    field("init", bytes(&init)),
                ]),
            )
            .unwrap();
            let body =
                loom::serde_json::to_vec(&object([field("action", Value::from("row"))])).unwrap();
            loom::perform::<()>(
                "actor.send",
                object([field("cap", bytes(&cap)), field("msg", bytes(&body))]),
            )
            .unwrap();
            let token: Value = loom::serde_json::from_slice(&cap).unwrap();
            let cap_id = token["cap_id"].as_u64().unwrap().to_string();
            loom::perform::<()>(
                "actor.revoke",
                object([field("cap_id", Value::from(cap_id))]),
            )
            .unwrap();
        }
        "unknown" => {
            // Swallowing a rejected root effect must not make a message commit.
            let _ = loom::perform::<Value>("nope", object([field("request", bytes(&[]))]));
        }
        "unknown_actor" => {
            let _ = loom::perform::<Value>("actor.nope", object([field("request", bytes(&[]))]));
        }
        "effect" => {
            insert(&msg);
            let _ = loom::perform::<Vec<u8>>(
                "test.effect",
                object([field("request", bytes(&[1, 2, 3]))]),
            );
        }
        "scoped_trap" => {
            loom::scope(|scope| {
                scope
                    .spawn(|| {
                        loom::perform::<Vec<u8>>(
                            "test.effect",
                            object([field("request", bytes(&[1, 2, 3]))]),
                        )
                        .unwrap();
                        std::panic::panic_any("scoped fixture panic");
                    })
                    .unwrap()
                    .join()
                    .unwrap();
            });
        }
        "handler_trap" => {
            loom::perform::<Vec<u8>>("test.effect", object([field("request", bytes(&[1, 2, 3]))]))
                .unwrap();
            let _ = loom::handle(
                ["local"],
                |_, _| std::panic::panic_any("handler fixture panic"),
                || loom::perform::<Value>("local", Value::Null),
            );
        }
        _ => std::panic::panic_any("unknown fixture action"),
    }
}

fn insert(msg: &[u8]) {
    let rows: Value = loom::perform(
        "sql",
        object([
            field(
                "sql",
                Value::from("INSERT INTO entries(body) VALUES (?) RETURNING body"),
            ),
            field("params", Value::Array(Vec::from([cell(msg)]))),
        ]),
    )
    .unwrap();
    check(rows["columns"] == Value::Array(Vec::from([Value::from("body")])));
    check(rows["rows"] == Value::Array(Vec::from([Value::Array(Vec::from([cell(msg)]))])));
}

struct Field {
    name: &'static str,
    value: Value,
}
fn field(name: &'static str, value: Value) -> Field {
    Field { name, value }
}
fn object<const N: usize>(fields: [Field; N]) -> Value {
    let mut object = loom::serde_json::Map::new();
    for field in fields {
        object.insert(field.name.to_owned(), field.value);
    }
    Value::Object(object)
}
fn bytes(value: &[u8]) -> Value {
    Value::Array(value.iter().copied().map(Value::from).collect())
}
fn cell(value: &[u8]) -> Value {
    object([
        field("type", Value::from("blob")),
        field("value", bytes(value)),
    ])
}
fn check(condition: bool) {
    if !condition {
        std::panic::panic_any("fixture assertion failed");
    }
}
