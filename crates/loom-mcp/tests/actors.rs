use std::sync::Arc;

use loom_actor::{Config, DefaultEffects, Node};
use loom_api::{Access, Service};
use loom_mcp::LoomMcp;
use loom_proto::Lang;
use loom_store::Store;
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, ReadResourceRequestParams, ResourceContents},
    service::RunningService,
};
use serde_json::{Value, json};

struct TransportRegistry;

#[async_trait::async_trait]
impl loom_actor::Registry for TransportRegistry {
    async fn resolve(&self, reference: &str) -> anyhow::Result<Arc<dyn loom_actor::Behavior>> {
        if reference == "trap-test" {
            return Ok(Arc::new(TrappingHandler));
        }
        anyhow::ensure!(reference == "counter-v1", "unknown behavior {reference}");
        Ok(Arc::new(loom_actor::builtin::Counter::plain()))
    }

    async fn behaviors(&self) -> anyhow::Result<Vec<loom_actor::builtin::BehaviorInfo>> {
        Ok(vec![loom_actor::builtin::BehaviorInfo {
            hash: "counter-v1".into(),
            description: "Transport test counter.".into(),
        }])
    }
}

struct TrappingHandler;
#[async_trait::async_trait]
impl loom_actor::Behavior for TrappingHandler {
    fn hash(&self) -> &str {
        "trap-test"
    }
    fn schema(&self) -> &str {
        ""
    }
    async fn handle(&self, _: &mut loom_actor::Ctx<'_>, _: &[u8]) -> Result<(), loom_actor::Trap> {
        Err(loom_actor::Trap::new("transport handler trapped"))
    }
}

struct Fixture {
    client: RunningService<RoleClient, ()>,
    server: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_access(Access::owner()).await
    }

    async fn with_access(access: Access) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let service = Arc::new(
            Service::new(
                Store::memory().unwrap(),
                directory.path().to_owned(),
                vec![Lang::Rust],
            )
            .unwrap(),
        );
        let node = Node::new(
            directory.path().join("actors"),
            Arc::new(TransportRegistry),
            Arc::new(DefaultEffects),
            Config::default(),
        )
        .await
        .unwrap();
        let transport = tokio::io::duplex(65536);
        let server = tokio::spawn(async move {
            LoomMcp::new(service, access, node)
                .serve(transport.0)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let client = ().serve(transport.1).await.unwrap();
        Self {
            client,
            server,
            _directory: directory,
        }
    }

    async fn envelope(&self, name: &str, args: Value) -> Value {
        let result = self.client.call_tool(request(name, args)).await.unwrap();
        assert_ne!(
            result.is_error,
            Some(true),
            "domain failures use the shared envelope"
        );
        let text = result
            .content
            .first()
            .and_then(|content| content.raw.as_text())
            .expect("JSON text tool response");
        let envelope: Value = serde_json::from_str(&text.text).unwrap();
        assert!(envelope["ok"].is_boolean(), "{envelope}");
        assert!(envelope["seq"].is_number(), "{envelope}");
        assert!(envelope.get("result").is_some(), "{envelope}");
        assert!(envelope["diagnostics"].is_array(), "{envelope}");
        envelope
    }

    async fn call(&self, name: &str, args: Value) -> Value {
        let envelope = self.envelope(name, args).await;
        assert_eq!(envelope["ok"], true, "{envelope}");
        envelope["result"].clone()
    }

    async fn counter(&self) -> String {
        let spawned = self
            .call("spawn", json!({"def":"counter-v1", "init":null}))
            .await;
        spawned["id"]
            .as_str()
            .expect("spawn returns actor id")
            .to_owned()
    }

    async fn send_three(&self, id: &str) {
        for seq in 1..=3 {
            let sent = self
                .call(
                    "send",
                    json!({"id":id,"key":format!("test:{seq}"),"msg":{"n":seq}}),
                )
                .await;
            assert_eq!(sent["cursor"], seq);
        }
    }

    async fn close(self) {
        self.client.cancel().await.unwrap();
        self.server.await.unwrap();
    }
}

fn request(name: &str, args: Value) -> CallToolRequestParams {
    CallToolRequestParams {
        meta: None,
        name: name.to_owned().into(),
        arguments: Some(args.as_object().unwrap().clone()),
        task: None,
    }
}

#[tokio::test]
async fn mcp_spawn_send_tree() {
    let fixture = Fixture::new().await;
    let tools = fixture.client.list_all_tools().await.unwrap();
    let actual: std::collections::BTreeSet<_> =
        tools.iter().map(|tool| tool.name.as_ref()).collect();
    let expected = loom_proto::verbs::VERBS
        .iter()
        .map(|verb| verb.name)
        .collect();
    assert_eq!(actual, expected);
    let found = fixture.call("find", json!({"text":"missing"})).await;
    assert!(found.as_array().unwrap().is_empty());
    let behaviors = fixture.call("behaviors", json!({})).await;
    let behavior = behaviors
        .as_array()
        .unwrap()
        .iter()
        .find(|behavior| behavior["hash"] == "counter-v1")
        .expect("transport behavior is discoverable");
    assert!(!behavior["description"].as_str().unwrap().is_empty());
    let id = fixture.counter().await;
    fixture.send_three(&id).await;
    let tree = fixture.call("tree", json!({})).await;
    assert_eq!(tree["behavior_hash"], "supervisor-v1");
    let child = tree["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|child| child["id"] == id)
        .expect("counter belongs to node root");
    assert_eq!(child["status"], "running");
    assert_eq!(child["behavior_hash"], "counter-v1");
    assert_eq!(child["cursor"], 3);
    let duplicate = fixture
        .call("send", json!({"id":id,"key":"test:3","msg":{"n":3}}))
        .await;
    assert_eq!(duplicate["cursor"], 3);
    fixture.close().await;
}

#[tokio::test]
async fn mcp_sql_refuses_writes() {
    let fixture = Fixture::new().await;
    let id = fixture.counter().await;
    struct RejectedQuery {
        query: &'static str,
        kind: &'static str,
    }
    for rejected in [
        RejectedQuery {
            query: "INSERT INTO entries(seq, body, implementation) VALUES (99, 'bad', 'counter-v1')",
            kind: "insert",
        },
        RejectedQuery {
            query: "/* inspection */ INSERT INTO entries(seq) VALUES (99)",
            kind: "insert",
        },
        RejectedQuery {
            query: "WITH seed(v) AS (VALUES(99)) INSERT INTO entries(seq) SELECT v FROM seed",
            kind: "insert",
        },
        RejectedQuery {
            query: "PRAGMA user_version=99",
            kind: "pragma",
        },
    ] {
        let response = fixture
            .envelope("sql", json!({"id":id,"query":rejected.query}))
            .await;
        assert_eq!(response["ok"], false, "{response}");
        let error = response["result"]["error"].as_str().unwrap();
        assert!(error.to_lowercase().contains(rejected.kind), "{error}");
        assert!(error.contains(&id), "{error}");
    }
    let response = fixture
        .envelope(
            "sql",
            json!({"id":id,"query":"SELECT 1; INSERT INTO entries(seq) VALUES (99)"}),
        )
        .await;
    assert_eq!(response["ok"], false, "{response}");
    let rows = fixture
        .call(
            "sql",
            json!({"id":id,"query":"SELECT seq FROM entries WHERE seq=?1","params":[99]}),
        )
        .await;
    assert!(rows.as_array().unwrap().is_empty());
    let read = fixture.call("sql", json!({"id":id,"query":"/* inspection */ WITH seed(v) AS (VALUES(7)) SELECT v FROM seed"})).await;
    assert_eq!(read, json!([{"v":7}]));
    fixture.close().await;

    let restricted = Fixture::with_access(Access::default()).await;
    for name in ["actors", "spawn"] {
        let args = if name == "spawn" {
            json!({"def":"counter-v1"})
        } else {
            json!({})
        };
        let response = restricted.envelope(name, args).await;
        assert_eq!(response["ok"], false, "{response}");
        assert!(
            response["result"]["error"]
                .as_str()
                .unwrap()
                .contains("scope required"),
            "{response}"
        );
    }
    restricted
        .client
        .read_resource(ReadResourceRequestParams {
            meta: None,
            uri: "actor://tree".into(),
        })
        .await
        .expect_err("actor resources require read access");
    restricted.close().await;
}

#[tokio::test]
async fn mcp_validate_returns_verdict() {
    let fixture = Fixture::new().await;
    let id = fixture.counter().await;
    fixture.send_three(&id).await;
    let result = fixture
        .call(
            "validate",
            json!({"id":id,"candidate":"counter-v1","k":3,"assertions":["SELECT COUNT(*)=3 FROM entries WHERE seq>0", "SELECT 0"]}),
        )
        .await;
    assert!(result["verdict"].get("Matched").is_some(), "{result}");
    assert_eq!(result["assertions"].as_array().unwrap().len(), 2);
    assert_eq!(result["assertions"][0]["passed"], true);
    assert_eq!(result["assertions"][1]["passed"], false);
    fixture.close().await;
}

#[tokio::test]
async fn mcp_resources_read() {
    let fixture = Fixture::new().await;
    let id = fixture.counter().await;
    fixture.send_three(&id).await;
    let uri = format!("actor://{id}/inbox");
    let resource = fixture
        .client
        .read_resource(ReadResourceRequestParams {
            meta: None,
            uri: uri.clone(),
        })
        .await
        .unwrap();
    assert_eq!(resource.contents.len(), 1);
    let ResourceContents::TextResourceContents {
        text,
        uri: returned_uri,
        mime_type,
        ..
    } = &resource.contents[0]
    else {
        panic!("expected JSON text resource")
    };
    assert_eq!(returned_uri, &uri);
    assert_eq!(mime_type.as_deref(), Some("application/json"));
    let rows: Value = serde_json::from_str(text).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row["seq"], index + 1);
    }
    fixture.close().await;
}

#[tokio::test]
async fn mcp_send_reports_handler_trap() {
    let fixture = Fixture::new().await;
    let spawned = fixture.call("spawn", json!({"def":"trap-test"})).await;
    let id = spawned["id"].as_str().unwrap();
    let response = fixture
        .envelope("send", json!({"id":id,"msg":{},"key":"trap"}))
        .await;
    assert_eq!(response["ok"], false, "{response}");
    assert_eq!(response["result"]["id"], id, "{response}");
    assert_eq!(response["result"]["seq"], 1);
    assert!(
        response["result"]["cause"]
            .as_str()
            .unwrap()
            .contains("transport handler trapped"),
        "{response}"
    );
    fixture.close().await;
}

#[tokio::test]
async fn mcp_actor_arguments_obey_protocol_integer_admission() {
    let fixture = Fixture::new().await;
    let response = fixture
        .envelope(
            "spawn",
            json!({"def":"counter-v1","init":9_007_199_254_740_992u64}),
        )
        .await;
    assert_eq!(response["ok"], false, "{response}");
    assert!(response["result"]["error"].is_string(), "{response}");
    let actors = fixture.call("actors", json!({})).await;
    assert!(
        !actors
            .as_array()
            .unwrap()
            .iter()
            .any(|actor| actor["behavior_hash"] == "counter-v1")
    );
    fixture.close().await;
}
