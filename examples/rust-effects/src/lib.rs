// This fixture exercises CAS plus delegated call capability checks.
pub fn exercise(effect: loom::Value) -> loom::Value {
    let Some(name) = effect["op"].as_str() else {
        return loom::serde_json::json!({"ok":false,"error":"effect name must be a string"});
    };
    let Some(args) = effect.get("args") else {
        return loom::serde_json::json!({"ok":false,"error":"effect args required"});
    };
    let result = match name {
        "cas.put" => loom::perform::<loom::Value>("cas.put", args.clone()),
        "call" => loom::perform::<loom::Value>("call", args.clone()),
        _ => return loom::serde_json::json!({"ok":false,"error":"unsupported effect"}),
    };
    match result {
        Ok(value) => loom::serde_json::json!({"ok":true,"value":value}),
        Err(error) => loom::serde_json::json!({"ok":false,"error":error}),
    }
}
