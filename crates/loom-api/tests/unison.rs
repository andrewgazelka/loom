use loom_api::Service;
use loom_proto::{CommandRequest, Lang, Value};
use loom_store::Store;
use serde_json::json;
use std::path::PathBuf;

fn service() -> Service {
    Service::new(
        Store::memory().unwrap(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )
    .unwrap()
}
async fn command(service: &Service, name: &str, args: Value) -> Value {
    let response = service
        .command(CommandRequest {
            session: None,
            command: name.into(),
            args,
        })
        .await;
    assert!(response.ok, "{response:?}");
    response.result
}
async fn add(service: &Service, source: &str) -> Value {
    command(service, "add", json!({"name":"answer","source":source})).await
}
const FIRST: &str = "pub fn main() -> i32 { let value = 41; value }";
const SECOND: &str = "pub fn main() -> i32 { let value = 42; value }";

#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn add_then_view_by_hash() {
    let service = service();
    let added = add(&service, FIRST).await;
    let viewed = command(&service, "view", json!({"target":added["hash"]})).await;
    assert_eq!(viewed["hash"], added["hash"]);
    assert_eq!(added["hash"], added["behavior_hash"]);
    assert_eq!(
        command(&service, "run", json!({"target":added["hash"]})).await["output"],
        41
    );
    assert_eq!(
        added["entries"]["main"]["effects"],
        json!({"labels":[],"unknown":false})
    );
    assert_eq!(viewed["entries"], added["entries"]);
    let hash = added["hash"].as_str().unwrap();
    let stored: Value =
        serde_json::from_str(&service.store.source(hash).unwrap().unwrap()).unwrap();
    assert_eq!(viewed["source"], stored["files"]["src/lib.rs"]);
    assert!(!viewed["items"].as_object().unwrap().is_empty());
    assert_eq!(viewed["behavior_hash"].as_str().unwrap().len(), 64);
    let found = command(&service, "find", json!({"text":"main"})).await;
    assert_eq!(found.as_array().unwrap().len(), 1);
}
#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn update_moves_name_keeps_old_runnable() {
    let service = service();
    let old = command(
        &service,
        "add",
        json!({"name":"answer","source":FIRST,"allowed_effects":[]}),
    )
    .await;
    let new = command(&service, "update", json!({"name":"answer","source":SECOND})).await;
    assert_ne!(old["hash"], new["hash"]);
    assert_eq!(new["def"]["allowed_effects"], json!([]));
    assert_eq!(
        command(&service, "run", json!({"target":"answer"})).await["output"],
        42
    );
    assert_eq!(
        command(&service, "run", json!({"target":old["hash"]})).await["output"],
        41
    );
}
#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn history_lists_changed_items() {
    let service = service();
    add(&service, FIRST).await;
    command(&service, "update", json!({"name":"answer","source":SECOND})).await;
    let history = command(&service, "history", json!({"name":"answer"})).await;
    assert_eq!(history.as_array().unwrap().len(), 2);
    assert!(history[0]["timestamp"].as_i64().unwrap() > 0);
    assert!(
        !history[1]["changes"]["changed"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn diff_reports_item_level_changes() {
    let service = service();
    let old = add(&service, FIRST).await;
    let renamed = command(
        &service,
        "update",
        json!({"name":"answer","source":FIRST.replace("value", "renamed")}),
    )
    .await;
    assert_eq!(old["hash"], renamed["hash"]);
    let history = command(&service, "history", json!({"name":"answer"})).await;
    assert_eq!(history.as_array().unwrap().len(), 1);
    let viewed = command(&service, "view", json!({"target":renamed["hash"]})).await;
    assert!(viewed["source"].as_str().unwrap().contains("renamed"));
    let diff = command(
        &service,
        "diff",
        json!({"old":old["hash"],"new":renamed["hash"]}),
    )
    .await;
    for key in ["added", "removed", "changed"] {
        assert!(diff[key].as_array().unwrap().is_empty(), "{diff}");
    }
    let new = add(&service, SECOND).await;
    assert_ne!(old["hash"], new["hash"]);
    let diff = command(
        &service,
        "diff",
        json!({"old":old["hash"],"new":new["hash"]}),
    )
    .await;
    assert_eq!(diff["changed"].as_array().unwrap().len(), 1, "{diff}");
}
#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn run_returns_output_and_effects() {
    let service = service();
    let added = add(
        &service,
        "pub fn main() -> i32 { loom::sleep(1).unwrap(); 42 }",
    )
    .await;
    assert_eq!(
        added["entries"]["main"]["effects"],
        json!({"labels":["sleep"],"unknown":false})
    );
    let result = command(&service, "run", json!({"target":"answer"})).await;
    assert_eq!(result["output"], 42);
    assert!(
        result["effects"].as_array().unwrap().iter().any(|effect| {
            effect["descriptor"]["op"] == "sleep" && effect["descriptor"]["args"]["ms"] == 1
        }),
        "{result}"
    );
}
#[tokio::test]
#[ignore = "requires Rust guest toolchain and LOOM_COMPILER_CACHE_OWNER"]
async fn dependents_follow_pins() {
    let service = service();
    let old = add(&service, FIRST).await;
    let dependent = command(&service,"add",json!({"name":"caller","source":"pub fn main() -> i32 { answer::main() }","deps":{"answer":old["hash"]}})).await;
    let new = command(&service, "update", json!({"name":"answer","source":SECOND})).await;
    assert_eq!(
        command(&service, "dependents", json!({"hash":old["hash"]})).await,
        json!([dependent["hash"]])
    );
    assert_eq!(
        command(&service, "dependents", json!({"hash":new["hash"]})).await,
        json!([])
    );
    assert_eq!(
        command(&service, "run", json!({"target":"caller"})).await["output"],
        41
    );
    let updated = command(
        &service,
        "update",
        json!({"name":"caller","source":"pub fn main() -> i32 { answer::main() + 1 }"}),
    )
    .await;
    assert_eq!(
        service
            .store
            .definition_deps(updated["hash"].as_str().unwrap())
            .unwrap()["answer"],
        old["hash"].as_str().unwrap()
    );
    assert_eq!(
        command(&service, "run", json!({"target":"caller"})).await["output"],
        42
    );
}
