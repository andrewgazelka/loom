//! Length-prefixed TCP. The listener leaves on driver cancellation/failure. Each
//! handle leaves on EOF, protocol/write error, or cancellation of its JoinSet.
use super::{Driver, DriverAck, DriverContext, DriverDelivery};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, tcp::OwnedReadHalf},
    sync::mpsc,
    task::JoinSet,
};

pub const HASH: &str = "tcp-listener-v1";
/// Both inbound and outbound frames are bounded before allocation/writing.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub struct TcpListenerDriver;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Init {
    bind: String,
}
struct ConnectionEnd {
    handle: String,
}
/// Closing a socket retires already-queued sends too; their pump rows must not
/// turn into lost-ack retries merely because EOF won the reader select.
struct PendingDeliveries {
    receiver: mpsc::Receiver<DriverDelivery>,
}
impl Drop for PendingDeliveries {
    fn drop(&mut self) {
        self.receiver.close();
        while let Ok(delivery) = self.receiver.try_recv() {
            delivery.acknowledge(Ok(DriverAck::Dropped));
        }
    }
}

#[async_trait]
impl Driver for TcpListenerDriver {
    fn hash(&self) -> &str {
        HASH
    }

    async fn run(&self, cx: DriverContext, init: &[u8], mut deliveries: mpsc::Receiver<DriverDelivery>) -> Result<()> {
        let init: Init = serde_json::from_slice(init)?;
        let listener = TcpListener::bind(&init.bind).await?;
        let listening = serde_json::to_vec(&serde_json::json!({"type":"listening","addr":listener.local_addr()?.to_string()}))?;
        cx.inject(cx.owner(), &format!("driver:{}:listening", cx.id()), &listening).await?;
        let mut connections = HashMap::<String, mpsc::Sender<DriverDelivery>>::new();
        // Drop aborts every owned connection future, including partial frames.
        let mut tasks = JoinSet::<Result<ConnectionEnd>>::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    stream.set_nodelay(true)?;
                    // A new socket has no resumable resource state. A ULID keeps
                    // its frame keys distinct across listener/node restarts.
                    let handle = ulid::Ulid::new().to_string();
                    let scoped = cx.for_handle(&handle)?;
                    let (send, receive) = mpsc::channel(64);
                    connections.insert(handle.clone(), send);
                    tasks.spawn(async move {
                        let result = connection(stream, &scoped, &handle, receive).await;
                        // connection() has dropped both socket halves before this
                        // event. Owners can use it as a handle-retirement witness.
                        let reason = result.err().map(|e| format!("{e:#}")).unwrap_or_else(|| "eof".into());
                        let msg = serde_json::to_vec(&serde_json::json!({"type":"closed","handle":handle,"reason":reason}))?;
                        scoped.inject(scoped.owner(), &format!("conn:{handle}:closed"), &msg).await?;
                        Ok(ConnectionEnd { handle })
                    });
                }
                delivery = deliveries.recv() => {
                    let Some(delivery) = delivery else { return Ok(()); };
                    match connections.get(&delivery.handle) {
                        None => delivery.acknowledge(Ok(DriverAck::Dropped)),
                        Some(connection) => match connection.try_send(delivery) {
                            Ok(()) => {},
                            Err(mpsc::error::TrySendError::Closed(delivery)) => delivery.acknowledge(Ok(DriverAck::Dropped)),
                            Err(mpsc::error::TrySendError::Full(delivery)) => delivery.acknowledge(Err(anyhow::anyhow!("TCP handle delivery queue full"))),
                        }
                    }
                }
                ended = tasks.join_next(), if !tasks.is_empty() => {
                    let ended = ended.context("connection task missing")???;
                    connections.remove(&ended.handle);
                }
            }
        }
    }
}

async fn read_frame(reader: &mut OwnedReadHalf) -> Result<Vec<u8>> {
    let length = reader.read_u32().await?;
    let length = usize::try_from(length)?;
    ensure!(length <= MAX_FRAME_BYTES, "TCP frame exceeds {MAX_FRAME_BYTES} bytes");
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn connection(stream: TcpStream, cx: &DriverContext, handle: &str, deliveries: mpsc::Receiver<DriverDelivery>) -> Result<()> {
    let mut deliveries = PendingDeliveries { receiver: deliveries };
    let (mut reader, mut writer) = stream.into_split();
    let mut frame_number = 0_u64;
    // Receipts live exactly as long as this socket. Once closed, all redeliveries
    // are acked-and-dropped and cannot write to a replacement connection.
    let mut delivered = HashSet::new();
    loop {
        // Keep a partial read alive while servicing outbound deliveries: restarting
        // read_exact after a select cancellation would corrupt frame boundaries.
        let read = read_frame(&mut reader);
        tokio::pin!(read);
        loop {
            tokio::select! {
                bytes = &mut read => {
                    let bytes = bytes?;
                    frame_number = frame_number.checked_add(1).context("TCP frame number exhausted")?;
                    cx.inject(cx.owner(), &format!("conn:{handle}:{frame_number}"), &bytes).await?;
                    break;
                }
                delivery = deliveries.receiver.recv() => {
                    let Some(delivery) = delivery else { return Ok(()); };
                    if delivered.contains(&delivery.key) {
                        delivery.acknowledge(Ok(DriverAck::Delivered));
                        continue;
                    }
                    if delivery.bytes.len() > MAX_FRAME_BYTES {
                        delivery.acknowledge(Err(anyhow::anyhow!("TCP frame exceeds {MAX_FRAME_BYTES} bytes")));
                        continue;
                    }
                    let mut frame = u32::try_from(delivery.bytes.len())?.to_be_bytes().to_vec();
                    frame.extend_from_slice(&delivery.bytes);
                    match tokio::time::timeout(Duration::from_secs(5), writer.write_all(&frame)).await {
                        Ok(Ok(())) => {
                            delivered.insert(delivery.key.clone());
                            delivery.acknowledge(Ok(DriverAck::Delivered));
                        }
                        _ => {
                            // Never retry a possibly partial frame on this stream.
                            // Close first, then drop this and any subsequent rows.
                            drop(writer);
                            delivery.acknowledge(Ok(DriverAck::Dropped));
                            return Err(anyhow::anyhow!("TCP write failed or timed out; handle closed"));
                        }
                    }
                }
            }
        }
    }
}
