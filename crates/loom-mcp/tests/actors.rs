use std::sync::Arc;

use loom_actor::{Config, DefaultEffects, Node, Registry};
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
            Registry::new(),
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

    async fn call(&self, name: &str, args: Value) -> Value {
        let result = self.client.call_tool(request(name, args)).await.unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let text = result
            .content
            .first()
            .and_then(|content| content.raw.as_text())
            .expect("JSON text tool response");
        serde_json::from_str(&text.text).unwrap()
    }

    async fn counter(&self) -> String {
        let spawned = self
            .call(
                "actor_spawn",
                json!({"behavior_hash":"counter-v1", "init":null}),
            )
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
                    "actor_send",
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
    let definitions: std::collections::BTreeSet<_> = tools
        .iter()
        .filter(|tool| tool.name.starts_with("loom_"))
        .map(|tool| tool.name.as_ref())
        .collect();
    assert_eq!(
        definitions,
        [
            "loom_add",
            "loom_view",
            "loom_update",
            "loom_history",
            "loom_diff",
            "loom_run",
            "loom_find",
            "loom_dependents",
            "loom_command"
        ]
        .into_iter()
        .collect()
    );
    assert!(!tools.iter().any(|tool| tool.name == "crate_add"));
    let found = fixture.call("loom_find", json!({"text":"missing"})).await;
    assert_eq!(found["ok"], true, "{found}");
    let actual: std::collections::BTreeSet<_> = tools
        .iter()
        .filter(|tool| tool.name.starts_with("actor_"))
        .map(|tool| tool.name.as_ref())
        .collect();
    let expected = [
        "actor_list",
        "actor_tree",
        "actor_info",
        "actor_send",
        "actor_spawn",
        "actor_stop",
        "actor_restart",
        "actor_promote",
        "actor_promote_where",
        "actor_lineage",
        "actor_dead_letters",
        "actor_fork",
        "actor_validate",
        "actor_sql",
        "actor_whereis",
        "actor_register",
        "actor_members",
        "actor_behaviors",
        "actor_run",
    ]
    .into_iter()
    .collect();
    assert_eq!(actual, expected);
    let behaviors = fixture.call("actor_behaviors", json!({})).await;
    for hash in ["counter-v1", "forwarder-v1", "echo-v1", "supervisor-v1"] {
        let behavior = behaviors
            .as_array()
            .unwrap()
            .iter()
            .find(|behavior| behavior["hash"] == hash)
            .expect("builtin behavior is discoverable");
        assert!(!behavior["description"].as_str().unwrap().is_empty());
    }
    let id = fixture.counter().await;
    fixture.send_three(&id).await;
    let tree = fixture.call("actor_tree", json!({})).await;
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
        .call("actor_send", json!({"id":id,"key":"test:3","msg":{"n":3}}))
        .await;
    assert_eq!(duplicate["cursor"], 3);
    let idle = fixture.call("actor_run", json!({})).await;
    assert_eq!(idle["processed"], 0);
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
        let error = fixture
            .client
            .call_tool(request(
                "actor_sql",
                json!({"id":id,"query":rejected.query}),
            ))
            .await
            .expect_err("writes must be MCP errors");
        assert!(
            error.to_string().to_lowercase().contains(rejected.kind),
            "{error}"
        );
        assert!(error.to_string().contains(&id), "{error}");
    }
    fixture
        .client
        .call_tool(request(
            "actor_sql",
            json!({"id":id,"query":"SELECT 1; INSERT INTO entries(seq) VALUES (99)"}),
        ))
        .await
        .expect_err("multiple statements must be refused");
    let rows = fixture
        .call(
            "actor_sql",
            json!({"id":id,"query":"SELECT seq FROM entries WHERE seq=?1","params":[99]}),
        )
        .await;
    assert!(rows.as_array().unwrap().is_empty());
    let read = fixture.call("actor_sql", json!({"id":id,"query":"/* inspection */ WITH seed(v) AS (VALUES(7)) SELECT v FROM seed"})).await;
    assert_eq!(read, json!([{"v":7}]));
    fixture.close().await;

    let restricted = Fixture::with_access(Access::default()).await;
    for denied in [
        request("actor_list", json!({})),
        request(
            "actor_spawn",
            json!({"behavior_hash":"counter-v1","init":null}),
        ),
    ] {
        let error = restricted
            .client
            .call_tool(denied)
            .await
            .expect_err("actor tools must retain access scopes");
        assert!(error.to_string().contains("scope required"), "{error}");
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
            "actor_validate",
            json!({"id":id,"candidate_hash":"counter-v1","k":3,"assertions":["SELECT COUNT(*)=3 FROM entries WHERE seq>0", "SELECT 0"]}),
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
        ..
    } = &resource.contents[0]
    else {
        panic!("expected JSON text resource")
    };
    assert_eq!(returned_uri, &uri);
    let rows: Value = serde_json::from_str(text).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row["seq"], index + 1);
    }
    fixture.close().await;
}
