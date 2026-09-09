#[loom::def]
pub fn add(left: i64, right: i64) -> i64 {
    left + right
}

#[cfg(test)]
mod tests {
    use loom::bindings::Guest;
    #[test]
    fn call_checks_arity_and_decodes_values() {
        assert_eq!(
            super::add_signature()["exports"][0]["returns"]["type"],
            "number"
        );
        assert_eq!(super::ADD_DEF.hash, "$self");
        assert_eq!(
            super::add_signature()["exports"][0]["effects"],
            loom::serde_json::json!({"labels":[],"unknown":true})
        );
        let signature: loom::TypeSig =
            loom::serde_json::from_value(super::add_signature()).unwrap();
        assert_eq!(signature.exports[0].params.len(), 2);
        let result =
            super::__LoomDefinition::call(vec![], loom::encode(&vec![12, 30]).unwrap()).unwrap();
        assert_eq!(loom::decode::<i64>(&result).unwrap(), 42);
        assert!(super::__LoomDefinition::call(vec![], loom::encode(&vec![12]).unwrap()).is_err());
        assert!(
            super::__LoomDefinition::call(vec![], loom::encode(&vec!["x", "y"]).unwrap()).is_err()
        );
    }

    #[test]
    fn named_arguments_encode_in_declaration_order_for_call_and_fork() {
        let call = loom::call_desc(
            super::ADD_DEF,
            super::AddArgs {
                right: 30,
                left: 12,
            },
        )
        .unwrap();
        let fork = loom::fork_desc(
            super::ADD_DEF,
            super::AddArgs {
                left: 12,
                right: 30,
            },
        )
        .unwrap();
        assert_eq!(
            call.args,
            loom::serde_json::json!({"def":"$self","args":[12,30]})
        );
        assert_eq!(fork.op, "fork");
        assert_eq!(call.args, fork.args);
        let result =
            super::__LoomDefinition::call(vec![], loom::encode(&call.args["args"]).unwrap())
                .unwrap();
        assert_eq!(loom::decode::<i64>(&result).unwrap(), 42);
        let _call = loom::call::<super::AddInvocation>;
        let _fork = loom::fork::<super::AddInvocation>;
    }
}
