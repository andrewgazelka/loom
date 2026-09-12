/// The native harness brackets each complete main invocation. Setup and removal
/// are included, making the result an upper bound on one full typed perform.
pub fn main() -> u64 {
    loom::handle(["bench.tick"], |_, _| loom::Reply::Resume(loom::serde_json::to_value(1).expect("encode value")), || {
        loom::perform::<u64>("bench.tick", loom::Value::Null).expect("tick")
    }).expect("handler")
}
