// This fixture exercises CAS plus delegated call capability checks.
#[loom::def(effects=["cas.put", "call"])]
pub fn exercise(effect: loom::Value) -> loom::Value {
    let Some(name) = effect["op"].as_str() else {
        return loom::serde_json::json!({"ok":false,"error":"effect name must be a string"});
    };
    let Some(args) = effect.get("args") else {
        return loom::serde_json::json!({"ok":false,"error":"effect args required"});
    };
    let result = loom::perform::<loom::Value>(name, args.clone());
    match result {
        Ok(value) => loom::serde_json::json!({"ok":true,"value":value}),
        Err(error) => loom::serde_json::json!({"ok":false,"error":error}),
    }
}
