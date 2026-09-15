//! Actor-owned sockets. Ingress and committed sends cross the existing Driver
//! boundary; neither a JavaScript isolate nor an HTTP handler owns their lifetime.
use crate::{HttpResponse, Scope, http::TenantService};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use axum::{
    Extension,
    extract::{
        Path, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use loom_actor::drivers::DriverReceipts;
use loom_actor::{Driver, DriverAck, DriverContext, DriverDelivery};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinSet, time::Instant};

pub const HASH: &str = "websocket-v1";
const PROTOCOL: &str = "loom.actor.v1";
const AUTH_PROTOCOL_PREFIX: &str = "loom.auth.";
const QUEUE_CAPACITY: usize = 64;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Browsers cannot set an Authorization header in `new WebSocket`. Carry the
/// bearer in an offered protocol, then negotiate only the public protocol so
/// the handshake response never reflects credentials. Authentication and tenant
/// selection remain in the ordinary middleware, before allocating a socket.
pub(crate) fn browser_bearer(headers: &HeaderMap) -> Result<Option<String>> {
    let mut encoded = None;
    let mut public_protocol = false;
    for header in headers.get_all("sec-websocket-protocol") {
        for protocol in header.to_str()?.split(',').map(str::trim) {
            if protocol == PROTOCOL {
                public_protocol = true;
            }
            if let Some(credential) = protocol.strip_prefix(AUTH_PROTOCOL_PREFIX) {
                ensure!(encoded.is_none(), "multiple WebSocket credentials supplied");
                ensure!(
                    !credential.is_empty() && credential.len() <= 5462,
                    "invalid WebSocket credential size"
                );
                encoded = Some(credential);
            }
        }
    }
    let Some(encoded) = encoded else {
        return Ok(None);
    };
    ensure!(
        public_protocol,
        "WebSocket credential requires loom.actor.v1 protocol"
    );
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .context("invalid WebSocket credential encoding")?;
    ensure!(
        !decoded.is_empty() && decoded.len() <= 4096,
        "invalid WebSocket credential size"
    );
    Ok(Some(
        String::from_utf8(decoded).context("WebSocket credential must be UTF-8")?,
    ))
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Limits {
    max_connections: usize,
    max_message_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 256,
            max_message_bytes: 1024 * 1024,
        }
    }
}

impl Limits {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=4096).contains(&self.max_connections),
            "invalid WebSocket connection limit"
        );
        ensure!(
            (1..=16 * 1024 * 1024).contains(&self.max_message_bytes),
            "invalid WebSocket message limit"
        );
        Ok(())
    }
}

struct Listener {
    driver: String,
    attachments: mpsc::Sender<WebSocket>,
    limits: Limits,
}

/// Each tenant Service owns its hub and registers `driver()` in its native
/// registry. Sharing a hub between tenants would share their resource namespace.
#[derive(Clone)]
pub struct WebSocketHub {
    listeners: Arc<Mutex<HashMap<String, Listener>>>,
    actor_message_limit: Arc<AtomicUsize>,
}

impl Default for WebSocketHub {
    fn default() -> Self {
        Self {
            listeners: Default::default(),
            actor_message_limit: Arc::new(AtomicUsize::new(usize::MAX)),
        }
    }
}

impl WebSocketHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn driver(&self) -> Arc<dyn Driver> {
        Arc::new(self.clone())
    }

    pub(crate) fn shares_resources(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.listeners, &other.listeners)
    }

    /// Bound serialized inbox events by the receiving sandbox's real message
    /// budget. Cloned registry drivers share this admission limit. Tightening it
    /// cannot grant a previously registered driver a larger resource budget.
    pub fn limit_actor_messages(&self, max_bytes: usize) {
        self.actor_message_limit
            .fetch_min(max_bytes, Ordering::Relaxed);
    }

    pub fn is_listening(&self, actor: &str) -> bool {
        self.listeners
            .lock()
            .expect("WebSocket listener registry poisoned")
            .get(actor)
            .is_some_and(|entry| !entry.attachments.is_closed())
    }

    /// Transfer an upgraded socket to its committed actor-owned listener.
    pub async fn attach(&self, actor: &str, socket: WebSocket) -> Result<()> {
        let sender = self
            .listeners
            .lock()
            .map_err(|_| anyhow::anyhow!("WebSocket listener registry poisoned"))?
            .get(actor)
            .context("actor has no WebSocket listener")?
            .attachments
            .clone();
        sender.try_send(socket).map_err(|_| {
            anyhow::anyhow!("WebSocket listener is closed or its attachment queue is full")
        })
    }

    fn limits(&self, actor: &str) -> Result<Limits> {
        self.listeners
            .lock()
            .map_err(|_| anyhow::anyhow!("WebSocket listener registry poisoned"))?
            .get(actor)
            .filter(|entry| !entry.attachments.is_closed())
            .map(|entry| entry.limits.clone())
            .context("actor has no WebSocket listener")
    }
}

struct Registration {
    hub: WebSocketHub,
    owner: String,
    driver: String,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut listeners) = self.hub.listeners.lock() {
            if listeners
                .get(&self.owner)
                .is_some_and(|entry| entry.driver == self.driver)
            {
                listeners.remove(&self.owner);
            }
        }
    }
}

pub(crate) async fn upgrade(
    Extension(tenant): Extension<TenantService>,
    Path(name): Path<String>,
    ws: WebSocketUpgrade,
) -> HttpResponse {
    if tenant.service.access.require(Scope::Execute).is_err() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(actors) = tenant.service.actors.as_ref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let actor = match actors.node.whereis(&name).await {
        Ok(Some(actor)) => actor,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let Some(hub) = tenant.service.websockets.clone() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let limits = match hub.limits(&actor) {
        Ok(limits) => limits,
        Err(error) => return (StatusCode::CONFLICT, error.to_string()).into_response(),
    };
    ws.protocols([PROTOCOL])
        .max_message_size(limits.max_message_bytes)
        .max_frame_size(limits.max_message_bytes)
        .on_upgrade(move |socket| async move {
            if let Err(error) = hub.attach(&actor, socket).await {
                eprintln!("actor {actor}: WebSocket attachment failed: {error:#}");
            }
        })
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Data {
    Text { text: String },
    Binary { bytes: Vec<u8> },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Command {
    Send { data: Data },
    Close { code: u16, reason: String },
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum Event<'a> {
    #[serde(rename = "websocket.listening")]
    Listening,
    #[serde(rename = "websocket.open")]
    Open {
        connection: &'a str,
        protocol: Option<String>,
    },
    #[serde(rename = "websocket.message")]
    Message { connection: &'a str, data: Data },
    #[serde(rename = "websocket.close")]
    Close {
        connection: &'a str,
        code: u16,
        reason: String,
        clean: bool,
    },
}

struct ConnectionEnd {
    handle: String,
}

struct Deliveries {
    receiver: mpsc::Receiver<DriverDelivery>,
}

impl Drop for Deliveries {
    fn drop(&mut self) {
        self.receiver.close();
        while let Ok(delivery) = self.receiver.try_recv() {
            delivery.acknowledge(Ok(DriverAck::Dropped));
        }
    }
}

#[async_trait]
impl Driver for WebSocketHub {
    fn hash(&self) -> &str {
        HASH
    }

    async fn run(
        &self,
        cx: DriverContext,
        init: &[u8],
        mut deliveries: mpsc::Receiver<DriverDelivery>,
    ) -> Result<()> {
        let limits: Limits = serde_json::from_slice(init)?;
        limits.validate()?;
        let (attachments, mut incoming) = mpsc::channel(QUEUE_CAPACITY);
        {
            let mut listeners = self
                .listeners
                .lock()
                .map_err(|_| anyhow::anyhow!("WebSocket listener registry poisoned"))?;
            ensure!(
                !listeners.contains_key(&cx.owner().target),
                "actor already owns a WebSocket listener"
            );
            listeners.insert(
                cx.owner().target.clone(),
                Listener {
                    driver: cx.id().into(),
                    attachments,
                    limits: limits.clone(),
                },
            );
        }
        let _registration = Registration {
            hub: self.clone(),
            owner: cx.owner().target.clone(),
            driver: cx.id().into(),
        };
        cx.inject(
            cx.owner(),
            &format!("websocket:{}:listening", cx.id()),
            &serde_json::to_vec(&Event::Listening)?,
        )
        .await?;
        let mut connections = HashMap::<String, mpsc::Sender<DriverDelivery>>::new();
        // JoinSet aborts every owned socket on driver cancellation. Nothing is
        // detached: stopping the actor closes its listener and all connections.
        let mut tasks = JoinSet::<Result<ConnectionEnd>>::new();
        loop {
            tokio::select! {
                socket = incoming.recv() => {
                    let Some(mut socket) = socket else { return Ok(()) };
                    if connections.len() >= limits.max_connections {
                        let _ = tokio::time::timeout(WRITE_TIMEOUT, socket.send(Message::Close(Some(CloseFrame { code: 1013, reason: "actor connection limit".into() })))).await;
                        continue;
                    }
                    let handle = uuid::Uuid::new_v4().to_string();
                    let scoped = cx.for_handle(&handle)?;
                    let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
                    connections.insert(handle.clone(), sender);
                    let limits = limits.clone();
                    let actor_message_limit = self.actor_message_limit.clone();
                    tasks.spawn(async move {
                        let closed = connection(socket, &scoped, &handle, receiver, &limits, &actor_message_limit).await;
                        let event = Event::Close { connection: &handle, code: closed.code, reason: closed.reason, clean: closed.clean };
                        scoped.inject(scoped.owner(), &format!("websocket:{handle}:closed"), &serde_json::to_vec(&event)?).await?;
                        Ok(ConnectionEnd { handle })
                    });
                }
                delivery = deliveries.recv() => {
                    let Some(delivery) = delivery else { return Ok(()) };
                    match connections.get(&delivery.handle) {
                        None => delivery.acknowledge(Ok(DriverAck::Dropped)),
                        Some(connection) => {
                            if let Err(error) = connection.send(delivery).await {
                                error.0.acknowledge(Ok(DriverAck::Dropped));
                            }
                        }
                    }
                }
                ended = tasks.join_next(), if !tasks.is_empty() => {
                    let ended = ended.context("WebSocket task missing")???;
                    connections.remove(&ended.handle);
                }
            }
        }
    }
}

struct Closed {
    code: u16,
    reason: String,
    clean: bool,
}

impl Closed {
    fn error(error: impl std::fmt::Display) -> Self {
        Self {
            code: 1006,
            reason: error.to_string(),
            clean: false,
        }
    }
}

async fn connection(
    socket: WebSocket,
    cx: &DriverContext,
    handle: &str,
    deliveries: mpsc::Receiver<DriverDelivery>,
    limits: &Limits,
    actor_message_limit: &AtomicUsize,
) -> Closed {
    match connection_inner(socket, cx, handle, deliveries, limits, actor_message_limit).await {
        Ok(closed) => closed,
        Err(error) => Closed::error(format!("{error:#}")),
    }
}

async fn connection_inner(
    socket: WebSocket,
    cx: &DriverContext,
    handle: &str,
    deliveries: mpsc::Receiver<DriverDelivery>,
    limits: &Limits,
    actor_message_limit: &AtomicUsize,
) -> Result<Closed> {
    let protocol = socket
        .protocol()
        .map(|value| value.to_str().map(str::to_owned))
        .transpose()?;
    let open = Event::Open {
        connection: handle,
        protocol,
    };
    cx.inject(
        cx.owner(),
        &format!("websocket:{handle}:open"),
        &serde_json::to_vec(&open)?,
    )
    .await?;
    let (mut writer, mut reader) = socket.split();
    let mut deliveries = Deliveries {
        receiver: deliveries,
    };
    let mut sequence = 0_u64;
    let mut delivered = DriverReceipts::new(1024);
    let mut closing: Option<Instant> = None;
    loop {
        tokio::select! {
            _ = async { match closing { Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await } } => {
                return Ok(Closed { code: 1006, reason: "WebSocket close handshake timed out".into(), clean: false });
            }
            message = reader.next() => {
                let Some(message) = message else { return Ok(Closed::error("WebSocket peer disconnected")); };
                let message = message?;
                let data = match message {
                    Message::Text(text) => Some(Data::Text { text: text.to_string() }),
                    Message::Binary(bytes) => Some(Data::Binary { bytes: bytes.to_vec() }),
                    Message::Close(frame) => {
                        // Tungstenite queues the peer's close reply; flush it
                        // without waiting for the actor to process its close event.
                        let clean = closing.is_some() || matches!(tokio::time::timeout(WRITE_TIMEOUT, writer.flush()).await, Ok(Ok(())));
                        return Ok(match frame {
                            Some(frame) => Closed { code: frame.code, reason: frame.reason.to_string(), clean },
                            None => Closed { code: 1005, reason: String::new(), clean },
                        });
                    }
                    Message::Ping(_) => {
                        tokio::time::timeout(WRITE_TIMEOUT, writer.flush()).await.context("WebSocket pong timed out")??;
                        None
                    }
                    Message::Pong(_) => None,
                };
                if let Some(data) = data {
                    if closing.is_some() { continue; }
                    sequence = sequence.checked_add(1).context("WebSocket event sequence exhausted")?;
                    let event = Event::Message { connection: handle, data };
                    let bytes = serde_json::to_vec(&event)?;
                    if bytes.len() > actor_message_limit.load(Ordering::Relaxed) {
                        // Admission happens before durable insertion: an oversized
                        // frame must not poison the actor's next mailbox turn.
                        let _ = tokio::time::timeout(WRITE_TIMEOUT, writer.send(Message::Close(Some(CloseFrame { code: 1009, reason: "actor message limit exceeded".into() })))).await;
                        return Ok(Closed { code: 1009, reason: "actor message limit exceeded".into(), clean: false });
                    }
                    cx.inject(cx.owner(), &format!("websocket:{handle}:{sequence}"), &bytes).await?;
                }
            }
            delivery = deliveries.receiver.recv() => {
                let Some(delivery) = delivery else { return Ok(Closed::error("WebSocket owner stopped")); };
                let receipt = match delivered.classify(&delivery.key) {
                    Ok(receipt) => receipt,
                    Err(error) => { delivery.acknowledge(Err(error)); continue; }
                };
                if delivered.contains(&receipt) {
                    delivery.acknowledge(Ok(DriverAck::Delivered));
                    continue;
                }
                if closing.is_some() {
                    delivery.acknowledge(Ok(DriverAck::Dropped));
                    continue;
                }
                let command = match parse_command(&delivery.bytes, limits) {
                    Ok(command) => command,
                    Err(error) => { delivery.acknowledge(Err(error)); continue; }
                };
                let message = match command {
                    Command::Send { data: Data::Text { text } } => Message::Text(text.into()),
                    Command::Send { data: Data::Binary { bytes } } => Message::Binary(bytes.into()),
                    Command::Close { code, reason } => {
                        closing = Some(Instant::now() + WRITE_TIMEOUT);
                        Message::Close(Some(CloseFrame { code, reason: reason.into() }))
                    }
                };
                match tokio::time::timeout(WRITE_TIMEOUT, writer.send(message)).await {
                    Ok(Ok(())) => {
                        delivered.commit(receipt);
                        delivery.acknowledge(Ok(DriverAck::Delivered));
                    }
                    _ => {
                        // A partial write cannot be retried safely. Retire the
                        // socket first; retries cannot target a replacement ID.
                        drop(writer);
                        delivery.acknowledge(Ok(DriverAck::Dropped));
                        return Ok(Closed::error("WebSocket write failed or timed out"));
                    }
                }
            }
        }
    }
}

fn parse_command(bytes: &[u8], limits: &Limits) -> Result<Command> {
    // JSON escapes a text control byte as six ASCII bytes; binary arrays use
    // at most four per byte. Bound the envelope before parsing either form.
    ensure!(
        bytes.len()
            <= limits
                .max_message_bytes
                .saturating_mul(6)
                .saturating_add(256),
        "WebSocket command exceeds byte limit"
    );
    let command: Command = serde_json::from_slice(bytes)?;
    match &command {
        Command::Send { data } => {
            let length = match data {
                Data::Text { text } => text.len(),
                Data::Binary { bytes } => bytes.len(),
            };
            ensure!(
                length <= limits.max_message_bytes,
                "WebSocket message exceeds byte limit"
            );
        }
        Command::Close { code, reason } => {
            ensure!(
                matches!(code, 1000..=1003 | 1007..=1009 | 1011..=1014 | 3000..=4999),
                "invalid WebSocket close code"
            );
            ensure!(
                reason.len() <= 123,
                "WebSocket close reason exceeds 123 bytes"
            );
        }
    }
    Ok(command)
}
