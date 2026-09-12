#[loom::actor(effects=[])]
pub struct Counter;
impl loom::Actor for Counter {
    type State = i64;
    type Event = i64;
    type Msg = i64;
    fn init() -> i64 {
        0
    }
    fn fold(state: i64, event: &i64) -> i64 {
        state + event
    }
    fn handle(_state: &i64, msg: i64) -> Vec<i64> {
        vec![msg]
    }
}

#[cfg(test)]
mod tests {
    use loom::core::Guest;
    #[test]
    fn handler_and_fold_share_cbor_state() {
        let state = loom::encode(&loom::Value::Null).unwrap();
        let events = super::Counter::run(state.clone(), loom::encode(&7).unwrap()).unwrap();
        assert_eq!(loom::decode::<Vec<i64>>(&events).unwrap(), vec![7]);
        let state = super::Counter::fold(state, loom::encode(&7).unwrap());
        assert_eq!(loom::decode::<i64>(&state).unwrap(), 7);
    }
}
