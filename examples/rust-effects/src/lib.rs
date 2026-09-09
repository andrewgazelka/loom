#[loom::def]
pub fn exercise(desc: loom::Value) -> loom::Value {
    let Some(op) = desc["op"].as_str() else {
        return loom::serde_json::json!({"ok":false,"error":"descriptor op must be a string"});
    };
    let Some(args) = desc.get("args") else {
        return loom::serde_json::json!({"ok":false,"error":"descriptor args required"});
    };
    let result = if op == "fork_join" {
        loom::perform::<loom::Value>(loom::Desc::new("fork", args.clone())).and_then(|fiber| {
            loom::perform::<loom::Value>(loom::Desc::new(
                "join",
                loom::serde_json::json!({"fibers":[fiber]}),
            ))
        })
    } else {
        loom::perform::<loom::Value>(loom::Desc::new(op, args.clone()))
    };
    match result {
        Ok(value) => loom::serde_json::json!({"ok":true,"value":value}),
        Err(error) => loom::serde_json::json!({"ok":false,"error":error}),
    }
}
