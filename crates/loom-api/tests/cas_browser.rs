use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use loom_api::{Authorizer, Scope, Service, TokenConfig};
use loom_proto::{
    CasInspection, CasPage, CommandRequest, DAG_CBOR_CODEC, Lang, RAW_CODEC, Response, Value,
};
use loom_store::Store;
use serde_json::json;
use std::{path::PathBuf, sync::Arc};
use tower::ServiceExt;

struct Fixture {
    store: Store,
    router: Router,
}
impl Fixture {
    fn new() -> Self {
        let store = Store::memory().unwrap();
        let service = Arc::new(
            Service::new(
                store.clone(),
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
                vec![Lang::Rust],
            )
            .unwrap(),
        );
        let authorizer = Authorizer::new(vec![
            TokenConfig {
                token: "reader".into(),
                scopes: [Scope::Read].into_iter().collect(),
            },
            TokenConfig {
                token: "runner".into(),
                scopes: [Scope::Execute].into_iter().collect(),
            },
        ])
        .unwrap();
        Self {
            store,
            router: loom_api::router(service, authorizer),
        }
    }
    async fn command(&self, token: &str, command: &str, args: Value) -> Reply {
        let mut reply = self.raw_command(token, command, args).await;
        if reply.response.ok
            && let Some(cid) = reply.response.result.get("$ref").and_then(Value::as_str)
        {
            let resolved = self
                .raw_command(token, "resolve", json!({"hash":cid}))
                .await;
            assert!(resolved.response.ok, "{:?}", resolved.response);
            reply.response.result = resolved.response.result;
        }
        reply
    }
    async fn raw_command(&self, token: &str, command: &str, args: Value) -> Reply {
        let body = serde_json::to_vec(&CommandRequest {
            session: None,
            command: command.into(),
            args,
        })
        .unwrap();
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/command")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let response =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        Reply { status, response }
    }
    async fn inspect(&self, hash: &str) -> CasInspection {
        let reply = self
            .command("reader", "cas.inspect", json!({"hash":hash}))
            .await;
        assert!(reply.response.ok, "{:?}", reply.response);
        serde_json::from_value(reply.response.result).unwrap()
    }
}
struct Reply {
    status: StatusCode,
    response: Response,
}

#[tokio::test]
async fn listing_filters_paginates_and_requires_read_scope() {
    let fixture = Fixture::new();
    let mut hashes = vec![
        fixture.store.put("browser-fixture", b"one").unwrap(),
        fixture.store.put("browser-fixture", b"two").unwrap(),
        fixture.store.put("browser-fixture", b"three").unwrap(),
    ];
    fixture.store.put("other-kind", b"other").unwrap();
    hashes.sort();
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let reply = fixture
            .command(
                "reader",
                "cas.list",
                json!({"kind":"browser-fixture","limit":1,"after":after}),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.response.ok);
        let page: CasPage = serde_json::from_value(reply.response.result).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].kind, "browser-fixture");
        seen.push(page.items[0].hash.clone());
        after = page.next_cursor;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(seen, hashes);
    let reply = fixture
        .command("reader", "cas.list", json!({"q":&hashes[1][..12]}))
        .await;
    let page: CasPage = serde_json::from_value(reply.response.result).unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].hash, hashes[1]);
    for args in [
        json!({"limit":1001}),
        json!({"limit":0}),
        json!({"after":"bad"}),
        json!({"q":"xyz"}),
    ] {
        assert!(
            !fixture
                .command("reader", "cas.list", args)
                .await
                .response
                .ok
        );
    }
    for command in ["cas.list", "cas.inspect"] {
        let denied = fixture
            .command("runner", command, json!({"hash":hashes[0]}))
            .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN);
        assert_eq!(denied.response.result["code"], "forbidden");
    }
    let response = fixture
        .router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/command")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"command":"cas.list","args":{}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn shared_bytes_keep_both_cids_and_inspect_selected_codec() {
    let fixture = Fixture::new();
    let value = json!({"answer":42});
    let bytes = loom_proto::encode(&value).unwrap();
    let hash = fixture.store.put("dual-codec", &bytes).unwrap();
    assert_eq!(fixture.store.put_value("dag-copy", &value).unwrap(), hash);
    let raw = fixture.store.reference(&hash, RAW_CODEC).unwrap()["$ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let dag = fixture.store.reference(&hash, DAG_CBOR_CODEC).unwrap()["$ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let page: CasPage = serde_json::from_value(
        fixture
            .command("reader", "cas.list", json!({"kind":"dual-codec"}))
            .await
            .response
            .result,
    )
    .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].codecs.len(), 2);
    assert_ne!(raw, dag);
    let raw_view = fixture.inspect(&raw).await;
    assert_eq!(raw_view.codec.code, RAW_CODEC);
    assert!(raw_view.value.is_none());
    assert_eq!(raw_view.entry.size, bytes.len() as u64);
    let dag_view = fixture.inspect(&dag).await;
    assert_eq!(dag_view.codec.code, DAG_CBOR_CODEC);
    assert_eq!(dag_view.value, Some(value));
    assert!(!dag_view.truncated);
    assert_eq!(fixture.inspect(&hash).await.codec.code, RAW_CODEC);
}

#[tokio::test]
async fn links_navigate_to_real_blocks_and_previews_are_bounded() {
    let fixture = Fixture::new();
    let leaf = fixture
        .store
        .put_value("leaf", &json!({"answer":42}))
        .unwrap();
    let reference = fixture.store.reference(&leaf, DAG_CBOR_CODEC).unwrap();
    let parent = fixture
        .store
        .put_value("parent", &json!({"a/b~c":reference}))
        .unwrap();
    let inspected = fixture.inspect(&parent).await;
    assert_eq!(inspected.links.len(), 1);
    assert_eq!(inspected.links[0].path, "/a~1b~0c");
    let linked = fixture.inspect(&inspected.links[0].cid).await;
    assert_eq!(linked.entry.hash, leaf);
    assert_eq!(linked.value.unwrap()["answer"], 42);
    let text = fixture
        .store
        .put("text-preview", &vec![b'x'; 32 * 1024])
        .unwrap();
    let text_view = fixture.inspect(&text).await;
    assert_eq!(text_view.text.unwrap().len(), 16 * 1024);
    assert!(text_view.truncated);
    assert_eq!(text_view.hex.len(), 512);
    let binary = fixture
        .store
        .put("component", &vec![0; 256 * 1024])
        .unwrap();
    let binary_view = fixture.inspect(&binary).await;
    assert!(binary_view.text.is_none());
    assert!(binary_view.truncated);
    assert_eq!(binary_view.hex.len(), 512);
    assert_eq!(binary_view.entry.size, 256 * 1024);
    let links = fixture
        .store
        .put_value("many-links", &json!(vec![reference; 150]))
        .unwrap();
    let link_view = fixture.inspect(&links).await;
    assert_eq!(link_view.links.len(), 128);
    assert!(link_view.truncated);
}

#[tokio::test]
async fn browsing_large_results_does_not_write_into_the_store() {
    let fixture = Fixture::new();
    for index in 0..100 {
        fixture
            .store
            .put_value("blob", &json!({"index":index}))
            .unwrap();
    }
    let request = loom_proto::CasListRequest {
        limit: 1000,
        ..Default::default()
    };
    let before = fixture.store.cas_list(&request).unwrap();
    for _ in 0..2 {
        let reply = fixture
            .raw_command("reader", "cas.list", json!({"limit":100}))
            .await;
        assert!(reply.response.ok);
        assert!(
            reply
                .response
                .result
                .get("items")
                .unwrap()
                .as_array()
                .unwrap()
                .len()
                >= 100
        );
    }
    let after = fixture.store.cas_list(&request).unwrap();
    assert_eq!(before.items.len(), after.items.len());
}

#[tokio::test]
async fn empty_raw_block_inspects_without_error() {
    let fixture = Fixture::new();
    let hash = fixture.store.put("blob", &[]).unwrap();
    let view = fixture.inspect(&hash).await;
    assert_eq!(view.entry.size, 0);
    assert_eq!(view.codec.code, RAW_CODEC);
    assert_eq!(view.text.as_deref(), Some(""));
    assert!(view.hex.is_empty());
    assert!(view.links.is_empty());
    assert!(view.value.is_none());
    assert!(!view.truncated);
}
