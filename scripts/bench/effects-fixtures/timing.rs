/// The native harness brackets each complete main invocation. Setup and removal
/// are included, making the result an upper bound on one full typed perform.
#[loom::def(effects=[])]
pub fn main() -> u64 {
    loom::handle_labels(["bench.tick"], |_, _| loom::Reply::Resume(loom::serde_json::json!(1)), || {
        loom::perform::<u64>(loom::Desc::new("bench.tick", loom::Value::Null)).expect("tick")
    }).expect("handler")
}
