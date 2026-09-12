pub fn answer() -> i64 {
    42
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom::core::Guest;
    #[test]
    fn zero_argument_call_uses_empty_positional_envelope() {
        let args = <AnswerInvocation as loom::Invocation>::arguments(AnswerArgs {}).unwrap();
        assert!(args.is_empty());
        let result = __LoomDefinition::call(vec![], loom::encode(&args).unwrap()).unwrap();
        assert_eq!(loom::decode::<i64>(&result).unwrap(), 42);
        let _call = loom::call::<AnswerInvocation>;
    }
}
