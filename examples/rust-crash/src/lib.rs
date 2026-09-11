#[loom::actor(effects=["exec"])]
pub struct Crash;
impl loom::Actor for Crash {
    type State = u64;
    type Event = u64;
    type Msg = loom::Value;
    fn init() -> u64 {
        0
    }
    fn fold(state: u64, event: &u64) -> u64 {
        state + event
    }
    fn handle(_state: &u64, msg: loom::Value) -> Vec<u64> {
        execute(
            loom::serde_json::json!({"program":"sh","args":["-c","printf x >> \"$1\"","loom-first",msg["first_path"]]}),
        );
        execute(
            loom::serde_json::json!({"program":"sh","args":["-c","printf ready > \"$1\"; while [ ! -e \"$2\" ]; do sleep .05; done","loom-gate",msg["ready_path"],msg["gate_path"]]}),
        );
        execute(
            loom::serde_json::json!({"program":"sh","args":["-c","printf x >> \"$1\"","loom-second",msg["second_path"]]}),
        );
        vec![1]
    }
}
fn execute(args: loom::Value) {
    let result = loom::exec(args).expect("exec transport failed");
    assert_eq!(result["code"], 0, "exec failed: {result}");
}
