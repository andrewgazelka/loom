#[loom::def]
pub fn invoke(hash: String, left: i64, right: i64) -> i64 {
    loom::perform(loom::Desc::new(
        "call",
        loom::serde_json::json!({"def":hash,"args":[left,right]}),
    ))
    .expect("TypeScript definition call failed")
}
