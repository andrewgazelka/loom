use loom_api::Service;
use loom_proto::{CommandRequest, Lang, Value};
use loom_store::Store;
use serde_json::json;
use std::path::PathBuf;

async fn command(service: &Service, verb: &str, args: Value) -> Value {
    let response = service
        .command(CommandRequest {
            session: None,
            command: verb.into(),
            args,
        })
        .await;
    assert!(response.ok, "{response:?}");
    response.result
}

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn secondary_entry_change_moves_root_and_preserves_entry_addresses() {
    let service = Service::new(
        Store::memory().unwrap(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )
    .unwrap();
    let original = "pub fn alpha() -> i32 { 11 } pub fn beta() -> i32 { 22 }";
    let first = command(&service, "add", json!({"name":"alpha","source":original})).await;
    let second = command(
        &service,
        "update",
        json!({"name":"alpha","source":original.replace("22", "23")}),
    )
    .await;
    assert_ne!(
        first["hash"], second["hash"],
        "secondary entry must contribute to the definition root"
    );
    assert_eq!(
        first["entries"]["alpha"]["hash"],
        second["entries"]["alpha"]["hash"]
    );
    assert_ne!(
        first["entries"]["beta"]["hash"],
        second["entries"]["beta"]["hash"]
    );
    let history = command(&service, "history", json!({"name":"alpha"})).await;
    assert_eq!(history.as_array().unwrap().len(), 2);
    assert_eq!(history[0]["hash"], first["hash"]);
    assert_eq!(history[1]["hash"], second["hash"]);
    for publication in [&first, &second] {
        let beta_output = if publication["hash"] == first["hash"] {
            22
        } else {
            23
        };
        for name in ["alpha", "beta"] {
            let target = &publication["entries"][name]["hash"];
            let run = command(&service, "run", json!({"target":target})).await;
            assert_eq!(run["entry"], name);
            assert_eq!(
                run["output"],
                if name == "alpha" { 11 } else { beta_output }
            );
            let view = command(&service, "view", json!({"target":target})).await;
            assert_eq!(view["entry"], name);
            assert_eq!(view["entries"][name]["hash"], *target);
        }
        let response = service
            .command(CommandRequest {
                session: None,
                command: "run".into(),
                args: json!({"target":publication["hash"]}),
            })
            .await;
        assert!(!response.ok);
        let error = response.result.to_string();
        assert!(error.contains("alpha") && error.contains("beta"), "{error}");
    }
    assert_eq!(
        command(&service, "run", json!({"target":"alpha"})).await["output"],
        11
    );
    assert_eq!(
        command(&service, "run", json!({"target":"beta"})).await["output"],
        23
    );
}
