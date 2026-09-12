use super::Server;
use loom_api::Access;
use loom_mcp::LoomMcp;
use rmcp::{ServiceExt, model::CallToolRequestParams};
use serde_json::{Value, json};

#[tokio::test]
async fn oversized_result_has_identical_http_cli_and_mcp_envelopes() {
    let server = Server::start().await;
    let spawned = server.invoke(&["spawn", "counter-v1"]).await;
    assert!(spawned.ok, "{spawned:?}");
    let id = spawned.result["id"].as_str().unwrap();
    let query = "SELECT printf('%09000d', 1) AS value";
    let args = json!({"id":id,"query":query});
    let http: Value = reqwest::Client::new()
        .post(format!("{}/v1/command", server.url))
        .bearer_auth("test")
        .json(&json!({"command":"sql","args":args}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cli = serde_json::to_value(server.invoke(&["sql", id, query]).await).unwrap();

    let transport = tokio::io::duplex(65536);
    let mcp = LoomMcp::new(server.service.clone(), Access::owner(), server.node.clone());
    let task = tokio::spawn(async move {
        mcp.serve(transport.0)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let client = ().serve(transport.1).await.unwrap();
    let result = client
        .call_tool(CallToolRequestParams {
            name: "sql".into(),
            arguments: args.as_object().cloned(),
            meta: None,
            task: None,
        })
        .await
        .unwrap();
    let mcp: Value = serde_json::from_str(&result.content[0].raw.as_text().unwrap().text).unwrap();
    assert_eq!(http["ok"], true, "{http}");
    assert_eq!(http, cli);
    assert_eq!(http, mcp);
    assert!(serde_json::to_vec(&http["result"]).unwrap().len() < 8192);
    let reference = http["result"]["$ref"].as_str().unwrap();
    let resolved = server
        .service
        .store
        .get_value::<Value>(reference)
        .unwrap()
        .unwrap();
    assert!(serde_json::to_vec(&resolved).unwrap().len() > 8192);
    client.cancel().await.unwrap();
    task.await.unwrap();
}
