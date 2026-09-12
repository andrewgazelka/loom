use super::Server;
use loom_api::Access;
use loom_mcp::LoomMcp;
use loom_proto::Response;
use rmcp::{ServiceExt, model::CallToolRequestParams};
use serde_json::json;

pub(super) async fn assert_verbatim(server: &Server, hash: &str, submitted: &str, cli: &Response) {
    let http: Response = reqwest::Client::new()
        .post(format!("{}/v1/command", server.url))
        .bearer_auth("test")
        .json(&json!({"command":"view","args":{"target":hash}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
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
            name: "view".into(),
            arguments: json!({"target":hash}).as_object().cloned(),
            meta: None,
            task: None,
        })
        .await
        .unwrap();
    let mcp: Response =
        serde_json::from_str(&result.content[0].raw.as_text().unwrap().text).unwrap();
    for response in [cli, &http, &mcp] {
        assert!(response.ok, "{response:?}");
        assert_eq!(
            response.result["source"].as_str().unwrap().as_bytes(),
            submitted.as_bytes(),
            "view must preserve submitted bytes, including blank lines, CRLF and comments"
        );
    }
    assert_eq!(
        server
            .service
            .store
            .source(hash)
            .unwrap()
            .unwrap()
            .as_bytes(),
        submitted.as_bytes()
    );
    assert_eq!(
        serde_json::to_value(cli).unwrap(),
        serde_json::to_value(&http).unwrap()
    );
    assert_eq!(
        serde_json::to_value(cli).unwrap(),
        serde_json::to_value(&mcp).unwrap()
    );
    client.cancel().await.unwrap();
    task.await.unwrap();
}
