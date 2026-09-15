use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use loom_actor::{Actor, Behavior, Cap, Config, Ctx, DefaultEffects, Driver, Io, Node, Trap};
use loom_api::{Authorizer, Scope, Service, TokenConfig, WebSocketHub};
use loom_proto::Lang;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{net::TcpStream, task::JoinHandle};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

const OWNER: &str = "websocket-test-owner";
const TRAPPING: &str = "websocket-trapping-owner";
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Owner {
    trap: bool,
}
#[async_trait]
impl Behavior for Owner {
    fn hash(&self) -> &str {
        if self.trap { TRAPPING } else { OWNER }
    }
    fn schema(&self) -> &str {
        "CREATE TABLE IF NOT EXISTS events(body TEXT, cap TEXT);"
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        if msg == b"start" {
            cx.spawn_driver("websocket-v1", b"{}").await?;
            return Ok(());
        }
        let event: Value = serde_json::from_slice(msg).map_err(|e| Trap::new(e.to_string()))?;
        if event["type"] == "down" {
            cx.spawn_driver("websocket-v1", b"{}").await?;
        }
        if event["type"] == "send-old" {
            let cap: Cap = serde_json::from_value(event["cap"].clone()).unwrap();
            cx.send(
                &cap,
                &serde_json::to_vec(
                    &json!({"type":"send", "data":{"type":"text", "text":"stale"}}),
                )
                .unwrap(),
            )
            .await?;
            return Ok(());
        }
        if event["type"] == "websocket.open"
            || event["type"] == "websocket.message"
            || event["type"] == "websocket.close"
        {
            let cap = cx.sender_cap().await?;
            cx.sql(
                "INSERT INTO events VALUES (?, ?)",
                [event.to_string(), serde_json::to_string(&cap).unwrap()],
            )
            .await?;
            if event["type"] == "websocket.message" {
                let reply = if event["data"]["text"] == "close-now" {
                    json!({"type":"close", "code":1000, "reason":"done"})
                } else {
                    json!({"type":"send", "data":event["data"]})
                };
                cx.send(&cap, &serde_json::to_vec(&reply).unwrap()).await?;
                if self.trap {
                    return Err(Trap::new("websocket send rolled back"));
                }
            }
        }
        Ok(())
    }
}
struct Registry {
    hub: WebSocketHub,
}
#[async_trait]
impl loom_actor::Registry for Registry {
    async fn resolve(&self, reference: &str) -> anyhow::Result<Arc<dyn Behavior>> {
        match reference {
            OWNER => Ok(Arc::new(Owner { trap: false })),
            TRAPPING => Ok(Arc::new(Owner { trap: true })),
            _ => anyhow::bail!("unknown test behavior {reference}"),
        }
    }
    async fn behaviors(&self) -> anyhow::Result<Vec<loom_actor::builtin::BehaviorInfo>> {
        Ok(vec![])
    }
    async fn resolve_driver(&self, hash: &str) -> anyhow::Result<Arc<dyn Driver>> {
        anyhow::ensure!(hash == "websocket-v1", "unknown driver {hash}");
        Ok(self.hub.driver())
    }
}
struct Fixture {
    directory: tempfile::TempDir,
    node: Node,
    owner: String,
    actor: Actor,
    hub: WebSocketHub,
    url: String,
    server: JoinHandle<()>,
}
impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let hub = WebSocketHub::new();
        let node = Self::node(directory.path(), hub.clone()).await;
        let owner = node.spawn_root(OWNER, b"start").await.unwrap();
        node.register("socket-owner", &owner).await.unwrap();
        let actor = node.open(&owner).await.unwrap();
        let service = Service::new(
            loom_store::Store::memory().unwrap(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            vec![Lang::Rust],
        )
        .unwrap()
        .with_actors(node.clone())
        .with_websockets(hub.clone());
        let auth = Authorizer::new(vec![
            TokenConfig {
                tenant: Default::default(),
                token: "runner".into(),
                scopes: [Scope::Execute].into_iter().collect(),
            },
            TokenConfig {
                tenant: Default::default(),
                token: "reader".into(),
                scopes: [Scope::Read].into_iter().collect(),
            },
        ])
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "ws://{}/v1/actors/socket-owner/websocket",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, loom_api::router(Arc::new(service), auth))
                .await
                .unwrap();
        });
        let fixture = Self {
            directory,
            node,
            owner,
            actor,
            hub,
            url,
            server,
        };
        fixture.listening().await;
        fixture
    }
    async fn node(path: &std::path::Path, hub: WebSocketHub) -> Node {
        Node::new(
            path,
            Arc::new(Registry { hub }),
            Arc::new(DefaultEffects),
            Config {
                io: Io::Syscall,
                ..Config::default()
            },
        )
        .await
        .unwrap()
    }
    async fn listening(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                self.node.run_until_idle().await.unwrap();
                if self.hub.is_listening(&self.owner) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("websocket listener did not start");
    }
    async fn connect(&self) -> Socket {
        let mut request = self.url.clone().into_client_request().unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer runner".parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }
    async fn events(&self, kind: &str, count: usize) -> Vec<Value> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                self.node.run_until_idle().await.unwrap();
                let rows = self
                    .actor
                    .inspect_sql("SELECT body FROM events ORDER BY rowid", vec![])
                    .await
                    .unwrap();
                let events: Vec<Value> = rows
                    .rows
                    .iter()
                    .map(|r| serde_json::from_str::<Value>(&r.get::<String>(0).unwrap()).unwrap())
                    .filter(|e| e["type"] == kind)
                    .collect();
                if events.len() >= count {
                    return events;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("websocket actor event did not commit")
    }
    async fn close(self) {
        self.server.abort();
        self.node.close().await.unwrap();
    }
}
async fn receive(socket: &mut Socket) -> Message {
    tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("websocket frame timed out")
        .expect("websocket ended")
        .unwrap()
}

#[tokio::test]
async fn unicode_binary_ping_and_reconnect_cross_committed_actor_turns() {
    let fixture = Fixture::new().await;
    let mut socket = fixture.connect().await;
    let opens = fixture.events("websocket.open", 1).await;
    assert!(opens[0]["protocol"].is_null());
    let first_connection = opens[0]["connection"].clone();
    let text = "λ snow 雪 hello 👋";
    socket.send(Message::Text(text.into())).await.unwrap();
    let messages = fixture.events("websocket.message", 1).await;
    assert_eq!(messages[0]["data"], json!({"type":"text", "text":text}));
    assert_eq!(receive(&mut socket).await, Message::Text(text.into()));
    let bytes = vec![0, 255, 128, 1];
    socket
        .send(Message::Binary(bytes.clone().into()))
        .await
        .unwrap();
    let messages = fixture.events("websocket.message", 2).await;
    assert_eq!(messages[1]["data"], json!({"type":"binary", "bytes":bytes}));
    assert_eq!(receive(&mut socket).await, Message::Binary(bytes.into()));
    socket
        .send(Message::Ping(b"native-ping".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(
        receive(&mut socket).await,
        Message::Pong(b"native-ping".to_vec().into())
    );
    socket
        .close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
            code: 1000.into(),
            reason: "goodbye".into(),
        }))
        .await
        .unwrap();
    let closes = fixture.events("websocket.close", 1).await;
    assert_eq!(closes[0]["connection"], first_connection);
    assert_eq!(closes[0]["code"], 1000);
    assert_eq!(closes[0]["reason"], "goodbye");
    assert_eq!(closes[0]["clean"], true);
    let mut reconnected = fixture.connect().await;
    let opens = fixture.events("websocket.open", 2).await;
    assert_ne!(opens[1]["connection"], first_connection);
    reconnected
        .send(Message::Text("after reconnect".into()))
        .await
        .unwrap();
    fixture.events("websocket.message", 3).await;
    assert_eq!(
        receive(&mut reconnected).await,
        Message::Text("after reconnect".into())
    );
    fixture.close().await;
}

#[tokio::test]
async fn rolled_back_send_stays_off_wire_and_retry_commits_once() {
    let fixture = Fixture::new().await;
    let mut socket = fixture.connect().await;
    fixture.events("websocket.open", 1).await;
    fixture
        .node
        .promote(&fixture.owner, TRAPPING, "test", "trap after send")
        .await
        .unwrap();
    socket
        .send(Message::Text("rollback witness".into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            fixture.node.run_until_idle().await.unwrap();
            let rows = fixture.actor.inspect_sql("SELECT error FROM dead_letters WHERE error LIKE '%websocket send rolled back%'", vec![]).await.unwrap();
            if !rows.rows.is_empty() { return; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("trapping turn did not run");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.next())
            .await
            .is_err(),
        "rolled-back send reached client or closed connection"
    );
    fixture
        .node
        .promote(&fixture.owner, OWNER, "test", "retry rolled-back turn")
        .await
        .unwrap();
    fixture.events("websocket.message", 1).await;
    assert_eq!(
        receive(&mut socket).await,
        Message::Text("rollback witness".into())
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.next())
            .await
            .is_err(),
        "duplicate send reached client"
    );
    fixture.close().await;
}

#[tokio::test]
async fn guest_close_reaches_native_client() {
    let fixture = Fixture::new().await;
    let mut socket = fixture.connect().await;
    fixture.events("websocket.open", 1).await;
    socket
        .send(Message::Text("close-now".into()))
        .await
        .unwrap();
    fixture.events("websocket.message", 1).await;
    let Message::Close(Some(close)) = receive(&mut socket).await else {
        panic!("expected close frame")
    };
    assert_eq!(u16::from(close.code), 1000);
    assert_eq!(close.reason, "done");
    // Reading the close queues tungstenite's acknowledgement; flush it so the
    // driver can record the completed handshake before the node shuts down.
    socket.flush().await.unwrap();
    let closes = fixture.events("websocket.close", 1).await;
    assert_eq!(closes[0]["code"], 1000);
    assert_eq!(closes[0]["reason"], "done");
    assert_eq!(closes[0]["clean"], true);
    fixture.close().await;
}

#[tokio::test]
async fn upgrade_requires_execute_authorization() {
    let fixture = Fixture::new().await;
    for token in [None, Some("reader")] {
        let mut request = fixture.url.clone().into_client_request().unwrap();
        if let Some(token) = token {
            request
                .headers_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
        }
        let error = tokio_tungstenite::connect_async(request).await.unwrap_err();
        let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
            panic!("expected authorization HTTP response")
        };
        assert_eq!(
            response.status().as_u16(),
            if token.is_some() { 403 } else { 401 }
        );
    }
    assert!(
        fixture
            .actor
            .inspect_sql("SELECT body FROM events", vec![])
            .await
            .unwrap()
            .rows
            .is_empty()
    );
    fixture.close().await;
}

#[tokio::test]
async fn node_reopen_restarts_listener_and_drops_old_connection_capability() {
    let mut fixture = Fixture::new().await;
    let mut old_socket = fixture.connect().await;
    let old_opens = fixture.events("websocket.open", 1).await;
    let old_cap: Cap = serde_json::from_str(
        &fixture
            .actor
            .inspect_sql("SELECT cap FROM events ORDER BY rowid LIMIT 1", vec![])
            .await
            .unwrap()
            .rows[0]
            .get::<String>(0)
            .unwrap(),
    )
    .unwrap();
    old_socket
        .send(Message::Text("before restart".into()))
        .await
        .unwrap();
    fixture.events("websocket.message", 1).await;
    assert_eq!(
        receive(&mut old_socket).await,
        Message::Text("before restart".into())
    );
    fixture.server.abort();
    fixture.node.close().await.unwrap();
    drop(old_socket);

    // The durable DOWN turn reopens the listener. Connection capabilities must
    // keep their old identity even when the same actor owns the new listener.
    fixture.hub = WebSocketHub::new();
    fixture.node = Fixture::node(fixture.directory.path(), fixture.hub.clone()).await;
    fixture.actor = fixture.node.open(&fixture.owner).await.unwrap();
    fixture.listening().await;
    let service = Service::new(
        loom_store::Store::memory().unwrap(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )
    .unwrap()
    .with_actors(fixture.node.clone())
    .with_websockets(fixture.hub.clone());
    let auth = Authorizer::new(vec![TokenConfig {
        tenant: Default::default(),
        token: "runner".into(),
        scopes: [Scope::Execute].into_iter().collect(),
    }])
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    fixture.url = format!(
        "ws://{}/v1/actors/socket-owner/websocket",
        listener.local_addr().unwrap()
    );
    fixture.server = tokio::spawn(async move {
        axum::serve(listener, loom_api::router(Arc::new(service), auth))
            .await
            .unwrap();
    });
    let mut socket = fixture.connect().await;
    let opens = fixture.events("websocket.open", 2).await;
    assert_ne!(opens[1]["connection"], old_opens[0]["connection"]);
    fixture
        .node
        .send(
            &fixture.owner,
            "send-to-old-socket",
            &serde_json::to_vec(&json!({"type":"send-old", "cap":old_cap})).unwrap(),
        )
        .await
        .unwrap();
    fixture.node.run_until_idle().await.unwrap();
    let drops = fixture
        .actor
        .inspect_sql(
            "SELECT value FROM meta WHERE key LIKE 'driver_drop:%'",
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(
        drops.rows.len(),
        1,
        "old connection send must reach terminal driver drop"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.next())
            .await
            .is_err(),
        "old capability reached a replacement socket"
    );
    socket
        .send(Message::Text("after restart".into()))
        .await
        .unwrap();
    fixture.events("websocket.message", 2).await;
    assert_eq!(
        receive(&mut socket).await,
        Message::Text("after restart".into())
    );
    fixture.close().await;
}

#[tokio::test]
async fn javascript_listener_survives_fresh_isolates_and_restores_socket_capability() {
    let directory = tempfile::tempdir().unwrap();
    let hub = WebSocketHub::new();
    let service = Service::new(
        loom_store::Store::memory().unwrap(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::JavaScript],
    )
    .unwrap()
    .with_native_drivers(vec![], vec![hub.driver()])
    .unwrap();
    let source = r#"
const LOOM_SCHEMA = "CREATE TABLE events(body TEXT); CREATE TABLE sockets(connection TEXT PRIMARY KEY, cap TEXT)";
let turnsInIsolate = 0;
const main = loom.messages.json(async event => {
    turnsInIsolate += 1;
    if (turnsInIsolate !== 1) throw new Error("actor reused a prior turn's isolate");
    if (event.type === "start") {
        await loom.websockets.listen();
        return;
    }
    await loom.sql("INSERT INTO events VALUES (?)", [JSON.stringify(event)]);
    if (event.type === "websocket.open") {
        const socket = await loom.websockets.sender();
        await loom.sql("INSERT INTO sockets VALUES (?, ?)", [event.connection, JSON.stringify(socket)]);
    }
    if (event.type === "websocket.message") {
        const rows = await loom.sql("SELECT cap FROM sockets WHERE connection = ?", [event.connection]);
        if (rows.length !== 1) throw new Error("persisted socket capability missing");
        const socket = loom.websockets.get(JSON.parse(rows[0].cap));
        if (event.data.type === "text") await socket.send(event.data.text);
        else await socket.sendBytes(event.data.bytes);
    }
});
"#;
    let admitted = service
        .command(loom_proto::CommandRequest {
            session: None,
            command: "add".into(),
            args: json!({"name":"isolate-websocket", "lang":"javascript", "source":source}),
        })
        .await;
    assert!(admitted.ok, "{admitted:?}");
    let node = Node::new(
        directory.path(),
        service.actor_registry(),
        Arc::new(DefaultEffects),
        Config {
            io: Io::Syscall,
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let owner = node
        .spawn_root(
            admitted.result["hash"].as_str().unwrap(),
            br#"{"type":"start"}"#,
        )
        .await
        .unwrap();
    node.register("socket-owner", &owner).await.unwrap();
    let actor = node.open(&owner).await.unwrap();
    let service = service
        .with_actors(node.clone())
        .with_websockets(hub.clone());
    let auth = Authorizer::new(vec![TokenConfig {
        tenant: Default::default(),
        token: "runner".into(),
        scopes: [Scope::Execute].into_iter().collect(),
    }])
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "ws://{}/v1/actors/socket-owner/websocket",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, loom_api::router(Arc::new(service), auth))
            .await
            .unwrap();
    });
    let fixture = Fixture {
        directory,
        node,
        owner,
        actor,
        hub,
        url,
        server,
    };
    fixture.listening().await;
    let mut socket = fixture.connect().await;
    fixture.events("websocket.open", 1).await;
    socket
        .send(Message::Text("fresh isolate 雪".into()))
        .await
        .unwrap();
    fixture.events("websocket.message", 1).await;
    assert_eq!(
        receive(&mut socket).await,
        Message::Text("fresh isolate 雪".into())
    );
    socket
        .send(Message::Binary(vec![0, 128, 255].into()))
        .await
        .unwrap();
    fixture.events("websocket.message", 2).await;
    assert_eq!(
        receive(&mut socket).await,
        Message::Binary(vec![0, 128, 255].into())
    );
    // 300 KiB of ASCII fits the actor payload limit. Expanding its bytes into
    // a second JSON array would exceed 1 MiB on either ingress or reply.
    let large_text = "x".repeat(300 * 1024);
    socket
        .send(Message::Text(large_text.clone().into()))
        .await
        .unwrap();
    let messages = fixture.events("websocket.message", 3).await;
    assert_eq!(messages[2]["data"]["text"], large_text);
    assert_eq!(receive(&mut socket).await, Message::Text(large_text.into()));

    // The wire frame fits the listener's 1 MiB limit. Its JSON event expands
    // high bytes to four characters each and must be refused before injection.
    socket
        .send(Message::Binary(vec![255; 400 * 1024].into()))
        .await
        .unwrap();
    let Message::Close(Some(close)) = receive(&mut socket).await else {
        panic!("oversized actor event must close the socket")
    };
    assert_eq!(u16::from(close.code), 1009);
    let mut reconnected = fixture.connect().await;
    fixture.events("websocket.open", 2).await;
    reconnected
        .send(Message::Text("healthy after oversized frame".into()))
        .await
        .unwrap();
    let messages = fixture.events("websocket.message", 4).await;
    assert_eq!(
        messages.len(),
        4,
        "oversized frame reached the actor event table"
    );
    assert_eq!(messages[3]["data"]["text"], "healthy after oversized frame");
    assert_eq!(
        receive(&mut reconnected).await,
        Message::Text("healthy after oversized frame".into())
    );
    let oversized_inbox = fixture
        .actor
        .inspect_sql("SELECT seq FROM inbox WHERE length(msg) > 1048576", vec![])
        .await
        .unwrap();
    assert!(
        oversized_inbox.rows.is_empty(),
        "oversized serialized event reached the durable inbox"
    );
    let info = fixture.node.info(&fixture.owner).await.unwrap();
    assert_eq!(info.status, loom_actor::Status::Running, "{}", info.reason);
    assert!(
        fixture
            .actor
            .inspect_sql("SELECT error FROM dead_letters", vec![])
            .await
            .unwrap()
            .rows
            .is_empty()
    );
    fixture.close().await;
}

#[tokio::test]
async fn browser_protocol_auth_selects_public_protocol_and_rejects_ambiguous_credentials() {
    let fixture = Fixture::new().await;
    // Browser WebSocket constructors cannot set Authorization. Only the public
    // protocol may be reflected in the server handshake response.
    let mut request = fixture.url.clone().into_client_request().unwrap();
    request.headers_mut().insert(
        "sec-websocket-protocol",
        "loom.actor.v1, loom.auth.cnVubmVy".parse().unwrap(),
    );
    let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(
        response.headers().get("sec-websocket-protocol").unwrap(),
        "loom.actor.v1"
    );
    let opens = fixture.events("websocket.open", 1).await;
    assert_eq!(opens[0]["protocol"], "loom.actor.v1");
    socket
        .send(Message::Text("browser auth 雪 👋".into()))
        .await
        .unwrap();
    fixture.events("websocket.message", 1).await;
    assert_eq!(
        receive(&mut socket).await,
        Message::Text("browser auth 雪 👋".into())
    );

    struct Denied {
        protocols: &'static str,
        authorization: Option<&'static str>,
        statuses: &'static [u16],
    }
    for case in [
        Denied {
            protocols: "loom.actor.v1, loom.auth.cmVhZGVy",
            authorization: None,
            statuses: &[403],
        },
        Denied {
            protocols: "loom.actor.v1, loom.auth.%%%",
            authorization: None,
            statuses: &[400, 401],
        },
        Denied {
            protocols: "loom.actor.v1, loom.auth.cnVubmVy",
            authorization: Some("Bearer runner"),
            statuses: &[400],
        },
        Denied {
            protocols: "loom.actor.v1, loom.auth.cnVubmVy, loom.auth.cmVhZGVy",
            authorization: None,
            statuses: &[400],
        },
    ] {
        let mut request = fixture.url.clone().into_client_request().unwrap();
        request
            .headers_mut()
            .insert("sec-websocket-protocol", case.protocols.parse().unwrap());
        if let Some(authorization) = case.authorization {
            request
                .headers_mut()
                .insert("authorization", authorization.parse().unwrap());
        }
        let error = tokio_tungstenite::connect_async(request).await.unwrap_err();
        let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
            panic!("expected credential rejection HTTP response")
        };
        assert!(
            case.statuses.contains(&response.status().as_u16()),
            "unexpected status {} for {}",
            response.status(),
            case.protocols
        );
        assert!(
            !response.headers().contains_key("sec-websocket-protocol"),
            "rejected credentials must not negotiate a protocol"
        );
    }
    fixture.close().await;
}

#[tokio::test]
async fn browser_websockets_with_same_actor_name_stay_inside_authenticated_tenant() {
    struct TenantSocket {
        service: Arc<Service>,
        node: Node,
        actor: Actor,
        owner: String,
        hub: WebSocketHub,
        protocol: &'static str,
        text: &'static str,
    }
    let directory = tempfile::tempdir().unwrap();
    let mut tenants = Vec::new();
    struct Identity {
        name: &'static str,
        protocol: &'static str,
        text: &'static str,
    }
    for identity in [
        Identity {
            name: "alice",
            protocol: "loom.actor.v1, loom.auth.YWxpY2U",
            text: "Alice private 雪",
        },
        Identity {
            name: "bob",
            protocol: "loom.actor.v1, loom.auth.Ym9i",
            text: "Bob private λ",
        },
    ] {
        let hub = WebSocketHub::new();
        let node = Fixture::node(&directory.path().join(identity.name), hub.clone()).await;
        let owner = node.spawn_root(OWNER, b"start").await.unwrap();
        node.register("socket-owner", &owner).await.unwrap();
        let actor = node.open(&owner).await.unwrap();
        let service = Arc::new(
            Service::new(
                loom_store::Store::memory().unwrap(),
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
                vec![Lang::Rust],
            )
            .unwrap()
            .with_tenant(loom_api::TenantId::new(identity.name).unwrap())
            .with_build_directory(directory.path().join(identity.name).join("build"))
            .with_actors(node.clone())
            .with_websockets(hub.clone()),
        );
        tenants.push(TenantSocket {
            service,
            node,
            actor,
            owner,
            hub,
            protocol: identity.protocol,
            text: identity.text,
        });
    }
    let services =
        loom_api::ServiceDirectory::new(tenants.iter().map(|tenant| tenant.service.clone()))
            .unwrap();
    let authorizer = Authorizer::new(
        ["alice", "bob"]
            .into_iter()
            .map(|name| TokenConfig {
                tenant: loom_api::TenantId::new(name).unwrap(),
                token: name.into(),
                scopes: [Scope::Execute].into_iter().collect(),
            })
            .collect(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "ws://{}/v1/actors/socket-owner/websocket",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, loom_api::router(services, authorizer))
            .await
            .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for tenant in &tenants {
                tenant.node.run_until_idle().await.unwrap();
            }
            if tenants
                .iter()
                .all(|tenant| tenant.hub.is_listening(&tenant.owner))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("tenant listeners did not start");
    let mut sockets = Vec::new();
    for tenant in &tenants {
        let mut request = url.clone().into_client_request().unwrap();
        request
            .headers_mut()
            .insert("sec-websocket-protocol", tenant.protocol.parse().unwrap());
        let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(
            response.headers().get("sec-websocket-protocol").unwrap(),
            "loom.actor.v1"
        );
        socket
            .send(Message::Text(tenant.text.into()))
            .await
            .unwrap();
        sockets.push(socket);
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut received = 0;
            for tenant in &tenants {
                tenant.node.run_until_idle().await.unwrap();
                let rows = tenant.actor.inspect_sql("SELECT body FROM events WHERE json_extract(body, '$.type')='websocket.message'", vec![]).await.unwrap();
                if !rows.rows.is_empty() { received += 1; }
            }
            if received == tenants.len() { return; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("tenant actor messages did not commit");
    let mut caps = Vec::new();
    let mut connections = Vec::new();
    for (tenant, socket) in tenants.iter().zip(&mut sockets) {
        assert_eq!(receive(socket).await, Message::Text(tenant.text.into()));
        let rows = tenant.actor.inspect_sql("SELECT body, cap FROM events WHERE json_extract(body, '$.type')='websocket.message'", vec![]).await.unwrap();
        assert_eq!(rows.rows.len(), 1, "tenant received another tenant's frame");
        let event: Value = serde_json::from_str(&rows.rows[0].get::<String>(0).unwrap()).unwrap();
        assert_eq!(event["data"]["text"], tenant.text);
        connections.push(event["connection"].clone());
        caps.push(serde_json::from_str::<Cap>(&rows.rows[0].get::<String>(1).unwrap()).unwrap());
    }
    assert_ne!(connections[0], connections[1]);
    assert_ne!(caps[0], caps[1]);
    assert!(
        tenants[0]
            .node
            .check_cap(&caps[1], loom_actor::Rights::SEND, "other-tenant-socket")
            .await
            .is_err()
    );
    assert!(
        tenants[1]
            .node
            .check_cap(&caps[0], loom_actor::Rights::SEND, "other-tenant-socket")
            .await
            .is_err()
    );
    server.abort();
    for tenant in tenants {
        tenant.node.close().await.unwrap();
    }
}
