use loom_api::Service;
use loom_proto::{CommandRequest, Lang};
use serde_json::{Value, json};

#[tokio::test]
async fn missing_definition_errors_preserve_user_reference() {
    let service = Service::new(
        loom_store::Store::memory().unwrap(),
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )
    .unwrap();
    struct Case {
        verb: &'static str,
        args: Value,
    }
    let reference = "missing/user-reference";
    for case in [
        Case {
            verb: "view",
            args: json!({"target":reference}),
        },
        Case {
            verb: "run",
            args: json!({"target":reference}),
        },
        Case {
            verb: "history",
            args: json!({"name":reference}),
        },
        Case {
            verb: "dependents",
            args: json!({"hash":reference}),
        },
        Case {
            verb: "diff",
            args: json!({"old":reference,"new":"another-missing"}),
        },
    ] {
        let response = service
            .command(CommandRequest {
                session: None,
                command: case.verb.into(),
                args: case.args,
            })
            .await;
        assert!(!response.ok, "{} returned success", case.verb);
        assert!(
            response.result["error"]
                .as_str()
                .unwrap()
                .contains(reference),
            "{response:?}"
        );
    }
}
