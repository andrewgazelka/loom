#[loom::def]
pub fn answer() -> i64 {
    42
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom::bindings::Guest;
    #[test]
    fn zero_argument_call_and_fork_use_empty_positional_envelope() {
        let call = loom::call_desc(ANSWER_DEF, AnswerArgs {}).unwrap();
        let fork = loom::fork_desc(ANSWER_DEF, AnswerArgs {}).unwrap();
        assert_eq!(
            call.args,
            loom::serde_json::json!({"def":"$self","args":[]})
        );
        assert_eq!(fork.op, "fork");
        assert_eq!(call.args, fork.args);
        let result =
            __LoomDefinition::call(vec![], loom::encode(&call.args["args"]).unwrap()).unwrap();
        assert_eq!(loom::decode::<i64>(&result).unwrap(), 42);
        // Compile the same generic functions that invoke the actual host import.
        let _call = loom::call::<AnswerInvocation>;
        let _fork = loom::fork::<AnswerInvocation>;
    }
}
