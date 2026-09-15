//! Native process I/O uses the shared process supervisor; actor commits own sends.
use super::{Driver, DriverAck, DriverContext, DriverDelivery, DriverReceipts};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use loom_process::{ProcessEvent, ProcessSandbox, ProcessSpec, Supervisor};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Register one instance per host-approved command. Neither init nor messages
/// can override executable, arguments, environment, or working directory.
pub struct ProcessDriver {
    hash: String,
    supervisor: Supervisor,
    spec: ProcessSpec,
    sandbox: ProcessSandbox,
}
impl ProcessDriver {
    pub fn new(hash: String, supervisor: Supervisor, spec: ProcessSpec, sandbox: ProcessSandbox) -> Self {
        Self { hash, supervisor, spec, sandbox }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Stdin { bytes: Vec<u8> },
    CloseStdin,
}

#[async_trait]
impl Driver for ProcessDriver {
    fn hash(&self) -> &str {
        &self.hash
    }

    async fn run(&self, cx: DriverContext, init: &[u8], deliveries: mpsc::Receiver<DriverDelivery>) -> Result<()> {
        ensure!(init.is_empty(), "process driver init must be empty; command is host configured");
        let session = self.supervisor.start_sandboxed_session(self.spec.clone(), &self.sandbox).await?;
        run_session(cx, session, deliveries, serde_json::json!({})).await
    }
}

pub(crate) async fn run_session(
    cx: DriverContext,
    mut session: loom_process::ProcessSession,
    mut deliveries: mpsc::Receiver<DriverDelivery>,
    metadata: serde_json::Value,
) -> Result<()> {
    let process_id = session.id().to_owned();
    let mut input = session.take_input()?;
    let mut started = metadata.as_object().cloned().unwrap_or_default();
    started.insert("type".into(), serde_json::json!("process.started"));
    started.insert("process_id".into(), serde_json::json!(process_id));
    let started = serde_json::to_vec(&started)?;
    cx.inject(cx.owner(), &format!("process:{}:started", cx.id()), &started).await?;
    let mut delivered = DriverReceipts::new(1024);
    loop {
        tokio::select! {
            event = session.next_event() => {
                if forward_event(&cx, &process_id, event).await? { return Ok(()); }
            }
            delivery = deliveries.recv() => {
                let Some(delivery) = delivery else { return Ok(()); };
                if delivery.handle != "root" {
                    delivery.acknowledge(Ok(DriverAck::Dropped));
                    continue;
                }
                let receipt = match delivered.classify(&delivery.key) {
                    Ok(receipt) => receipt,
                    Err(error) => { delivery.acknowledge(Err(error)); continue; }
                };
                if delivered.contains(&receipt) {
                    delivery.acknowledge(Ok(DriverAck::Delivered));
                    continue;
                }
                let command: Command = match serde_json::from_slice(&delivery.bytes) {
                    Ok(command) => command,
                    Err(error) => {
                        delivery.acknowledge(Err(error.into()));
                        continue;
                    }
                };
                // Separate input ownership lets us drain stdout/stderr
                // while a large stdin write waits for the child to read.
                let operation = async {
                    match command {
                        Command::Stdin { bytes } => input.write(&bytes).await,
                        Command::CloseStdin => input.close_stdin().await,
                    }
                };
                tokio::pin!(operation);
                let result = loop {
                    tokio::select! {
                        result = &mut operation => break result,
                        event = session.next_event() => {
                            if forward_event(&cx, &process_id, event).await? {
                                delivery.acknowledge(Ok(DriverAck::Dropped));
                                return Ok(());
                            }
                        }
                    }
                };
                match result {
                    Ok(()) => {
                        delivered.commit(receipt);
                        delivery.acknowledge(Ok(DriverAck::Delivered));
                    }
                    Err(error) => {
                        // An I/O error may follow a partial stdin write. End
                        // this process before retiring the delivery; a retry
                        // must not append its bytes a second time.
                        drop(session);
                        delivery.acknowledge(Ok(DriverAck::Dropped));
                        return Err(error).context("process stdin failed; process closed");
                    }
                }
            }
        }
    }
}

async fn forward_event(cx: &DriverContext, process_id: &str, event: Option<ProcessEvent>) -> Result<bool> {
    let event = event.context("process event stream ended before exit")?;
    match event {
        ProcessEvent::Output { sequence, stderr, bytes } => {
            let message = serde_json::to_vec(&serde_json::json!({
                "type":"process.output", "process_id":process_id,
                "stream":if stderr {"stderr"} else {"stdout"}, "bytes":bytes
            }))?;
            cx.inject(cx.owner(), &format!("process:{}:output:{sequence}", cx.id()), &message).await?;
            Ok(false)
        }
        ProcessEvent::Exit { state } => {
            let message = serde_json::to_vec(&serde_json::json!({
                "type":"process.exit", "process_id":process_id,
                "phase":state.phase, "code":state.code, "error":state.error
            }))?;
            cx.inject(cx.owner(), &format!("process:{}:exit", cx.id()), &message).await?;
            Ok(true)
        }
    }
}
