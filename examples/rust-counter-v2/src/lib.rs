#[loom::actor]
pub struct Counter;
impl loom::Actor for Counter {
    type State = i64;
    type Event = i64;
    type Msg = i64;
    fn init() -> i64 {
        0
    }
    fn fold(state: i64, event: &i64) -> i64 {
        state + event * 2
    }
    fn handle(_state: &i64, msg: i64) -> Vec<i64> {
        vec![msg]
    }
}
