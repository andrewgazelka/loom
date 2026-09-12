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
                        require(effect.name == "local", "unexpected local effect");
                        loom::Reply::Resume(Value::from(17))
                    },
                    || {
                        loom::scope(|scope| {
                            let child = scope
                                .spawn(|| {
                                    let result: i64 =
                                        loom::perform("local", object([], [])).unwrap();
                                    require(result == 17, "unexpected local result");
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
                loom::perform("actor.cap", object(["cap_id"], [Value::from(cap_id)])).unwrap();
            require(restored == cap.token, "capability token changed");
            let body = loom::serde_json::to_vec(&object(
                ["action", "forwarded"],
                [Value::from("row"), Value::from(true)],
            ))
            .unwrap();
            loom::actor::send(&cap, &body).unwrap();
            require(request["trap"].as_bool() != Some(true), "trap after send");
        }
        "forged_send" => {
            let body = loom::serde_json::to_vec(&object(["action"], [Value::from("row")])).unwrap();
            // Ignoring a capability rejection must still abort the transaction.
            let _ = loom::perform::<()>(
                "actor.send",
                object(["cap", "msg"], [request["cap"].clone(), bytes(&body)]),
            );
        }
        "spawn_send" => {
            let init =
                loom::serde_json::to_vec(&object(["action"], [Value::from("init")])).unwrap();
            let cap: Vec<u8> = loom::perform(
                "actor.spawn",
                object(
                    ["behavior_hash", "init"],
                    [request["behavior_hash"].clone(), bytes(&init)],
                ),
            )
            .unwrap();
            let body = loom::serde_json::to_vec(&object(["action"], [Value::from("row")])).unwrap();
            loom::perform::<()>(
                "actor.send",
                object(["cap", "msg"], [bytes(&cap), bytes(&body)]),
            )
            .unwrap();
            let token: Value = loom::serde_json::from_slice(&cap).unwrap();
            let cap_id = token["cap_id"].as_u64().unwrap().to_string();
            loom::perform::<()>("actor.revoke", object(["cap_id"], [Value::from(cap_id)])).unwrap();
        }
        "unknown" => {
            // Swallowing a rejected root effect must not make a message commit.
            let _ = loom::perform::<Value>("nope", object(["request"], [bytes(&[])]));
        }
        "unknown_actor" => {
            let _ = loom::perform::<Value>("actor.nope", object(["request"], [bytes(&[])]));
        }
        "effect" => {
            insert(&msg);
            let _ =
                loom::perform::<Vec<u8>>("test.effect", object(["request"], [bytes(&[1, 2, 3])]));
        }
        "scoped_trap" => {
            loom::scope(|scope| {
                scope
                    .spawn(|| {
                        loom::perform::<Vec<u8>>(
                            "test.effect",
                            object(["request"], [bytes(&[1, 2, 3])]),
                        )
                        .unwrap();
                        fail("scoped fixture panic");
                    })
                    .unwrap()
                    .join()
                    .unwrap();
            });
        }
        "handler_trap" => {
            loom::perform::<Vec<u8>>("test.effect", object(["request"], [bytes(&[1, 2, 3])]))
                .unwrap();
            let _ = loom::handle(
                ["local"],
                |_, _| fail("handler fixture panic"),
                || loom::perform::<Value>("local", Value::Null),
            );
        }
        _ => fail("unknown fixture action"),
    }
}

fn insert(msg: &[u8]) {
    let rows: Value = loom::perform(
        "sql",
        object(
            ["sql", "params"],
            [
                Value::from("INSERT INTO entries(body) VALUES (?) RETURNING body"),
                Value::Array([blob(msg)].into()),
            ],
        ),
    )
    .unwrap();
    require(
        rows["columns"] == Value::Array([Value::from("body")].into()),
        "unexpected SQL columns",
    );
    require(
        rows["rows"] == Value::Array([Value::Array([blob(msg)].into())].into()),
        "unexpected SQL rows",
    );
}

fn object<const N: usize>(keys: [&str; N], values: [Value; N]) -> Value {
    let mut object = loom::serde_json::Map::new();
    let mut values = values.into_iter();
    for key in keys {
        object.insert(key.to_owned(), values.next().unwrap());
    }
    Value::Object(object)
}

fn bytes(value: &[u8]) -> Value {
    loom::serde_json::to_value(value).unwrap()
}

fn blob(value: &[u8]) -> Value {
    object(["type", "value"], [Value::from("blob"), bytes(value)])
}

fn require(condition: bool, message: &str) {
    if !condition {
        fail(message);
    }
}

fn fail(message: &str) -> ! {
    std::panic::panic_any(message.to_owned())
}
