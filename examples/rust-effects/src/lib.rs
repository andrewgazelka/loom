// This fixture exercises CAS plus delegated isolated-call capability checks.
// `{"op":"call","args":{"def":<hash>,"args":[value]}}` runs `exercise` of the
// named definition through `loom::isolated::call`; the target hash is only
// known at run time, so it goes through `Def::from_hex`.
pub fn exercise(effect: loom::Value) -> loom::Value {
    let Some(name) = effect["op"].as_str() else {
        return loom::serde_json::json!({"ok":false,"error":"effect name must be a string"});
    };
    let Some(args) = effect.get("args") else {
        return loom::serde_json::json!({"ok":false,"error":"effect args required"});
    };
    let result = match name {
        "cas.put" => loom::perform::<loom::Value>("cas.put", args.clone()),
        "call" => delegated(args),
        _ => return loom::serde_json::json!({"ok":false,"error":"unsupported effect"}),
    };
    match result {
        Ok(value) => loom::serde_json::json!({"ok":true,"value":value}),
        Err(error) => loom::serde_json::json!({"ok":false,"error":error}),
    }
}

fn delegated(args: &loom::Value) -> Result<loom::Value, String> {
    let Some(hash) = args["def"].as_str() else {
        return Err("call def must be a string".into());
    };
    let Some(argument) = args["args"]
        .as_array()
        .and_then(|arguments| arguments.first())
    else {
        return Err("call args must hold one positional value".into());
    };
    let def = loom::isolated::Def::<fn(loom::Value) -> loom::Value>::from_hex(hash)
        .map_err(|error| error.to_string())?
        .entry("exercise");
    loom::isolated::call(def, argument.clone()).map_err(|error| error.to_string())
}
