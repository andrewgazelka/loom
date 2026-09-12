#[loom::actor(effects=["actor.accept", "actor.send"])]
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
        let cap = loom::serde_json::from_value::<loom::actor::Cap>(msg["cap"].clone())
            .expect("cap must contain capability token bytes");
        let cap = loom::actor::accept(cap).expect("capability acceptance failed");
        let remaining = msg["remaining"]
            .as_u64()
            .expect("remaining must be a nonnegative integer");
        if remaining > 0 {
            loom::actor::send(
                &cap,
                &loom::serde_json::to_vec(
                    &loom::serde_json::json!({"cap":cap,"remaining":remaining-1}),
                )
                .expect("message serialization failed"),
            )
            .expect("self-send enqueue failed");
        }
        vec![1]
    }
}
