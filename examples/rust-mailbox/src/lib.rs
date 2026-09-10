#[loom::actor(effects=["send"])]
pub struct Mailbox;
impl loom::Actor for Mailbox {
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
        let actor = msg["actor"].as_str().expect("actor must be a string");
        let remaining = msg["remaining"]
            .as_u64()
            .expect("remaining must be a nonnegative integer");
        if remaining > 0 {
            loom::abilities::send(
                actor,
                loom::serde_json::json!({"actor":actor,"remaining":remaining-1}),
            )
            .expect("self-send enqueue failed");
        }
        vec![1]
    }
}
