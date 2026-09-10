/// Exercise a link through both the host CAS and the other guest language.
#[loom::def(effects=["cas.get", "call"])]
pub fn dag(payload: loom::Value, target: String) -> loom::Value {
    if target.is_empty() {
        return payload;
    }
    let Some(hash) = payload["$ref"].as_str() else {
        return loom::serde_json::json!({"stage":"input","error":"payload must be a link"});
    };
    let value: loom::Value = match loom::perform(loom::Desc::new(
        "cas.get",
        loom::serde_json::json!({"hash": hash}),
    )) {
        Ok(value) => value,
        Err(error) => return loom::serde_json::json!({"stage":"cas.get","error":error}),
    };
    let echo: loom::Value = match loom::perform(loom::Desc::new(
        "call",
        loom::serde_json::json!({"def": target, "args": [payload, ""]}),
    )) {
        Ok(value) => value,
        Err(error) => return loom::serde_json::json!({"stage":"cross-language call","error":error}),
    };
    loom::serde_json::json!({"reference": payload, "value": value, "echo": echo})
}
