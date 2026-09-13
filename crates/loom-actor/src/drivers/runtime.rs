//! Worker cancellation drops resources; the broker finishes admitted injections.
use super::{DriverAck, DriverContext, DriverDelivery, DriverSpawn, Injection, target};
use crate::{Node, Rights, Status, actor};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{mpsc, oneshot, watch};

/// Upper bound on waiting for an aborted driver worker to finish.
const CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Open entries leave when the worker finishes (run completion, explicit stop,
/// owner stop, node close/drop): the broker removes the entry. Spawn retry never
/// opens a second resource because the owner's `driver_spawn:<id>` receipt is
/// checked first; a new committed spawn gets a new ID.
struct Running {
    owner: String,
    deliveries: mpsc::Sender<DriverDelivery>,
    abort: tokio::task::AbortHandle,
    closed: watch::Receiver<bool>,
    intentional: Arc<AtomicBool>,
}
#[derive(Default)]
pub(crate) struct Drivers {
    running: Mutex<HashMap<String, Running>>,
}
/// Brokers hold no lifetime owner, so dropping the last public Node aborts workers.
pub(crate) struct DriverLifetime(pub Arc<Drivers>);
impl Drop for DriverLifetime {
    fn drop(&mut self) {
        if let Ok(running) = self.0.running.lock() {
            for driver in running.values() {
                driver.intentional.store(true, Ordering::Release);
                driver.abort.abort();
            }
        }
    }
}
struct Completion(watch::Sender<bool>);
impl Drop for Completion {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

impl Node {
    /// Closed-handle drops are terminal metadata, not a second delivery path.
    /// The ordinary pump marks the row delivered after this marker commits.
    async fn driver_drop(&self, sender: &str, destination: &str, key: &str) -> Result<()> {
        let source = self.open_actor(sender).await?;
        let mut conn = source.conn.lock().await;
        let tx = conn.transaction().await?;
        actor::set_meta(&tx, &format!("driver_drop:{key}"), destination).await?;
        self.commit_control(sender, tx).await
    }

    pub(crate) async fn deliver_driver(&self, sender: &str, destination: &str, bytes: &[u8], key: &str) -> Result<()> {
        if let Some(destination) = destination.strip_prefix("drv:spawn:") {
            let spawn: DriverSpawn = serde_json::from_slice(bytes)?;
            ensure!(destination == format!("drv:{}:root", spawn.id), "driver spawn target mismatch");
            ensure!(target(destination)?.owner == sender && spawn.owner.target == sender, "driver spawn owner mismatch");
            return self.open_driver(sender, spawn).await;
        }
        let target = target(destination)?;
        let deliveries = self
            .drivers
            .running
            .lock()
            .map_err(|_| anyhow::anyhow!("driver registry poisoned"))?
            .get(target.id)
            .filter(|entry| !*entry.closed.borrow())
            .map(|entry| entry.deliveries.clone());
        let Some(deliveries) = deliveries else {
            return self.driver_drop(sender, destination, key).await;
        };
        let (ack, result) = oneshot::channel();
        if deliveries.send(DriverDelivery { handle: target.handle.into(), key: key.into(), bytes: bytes.into(), ack }).await.is_err() {
            return self.driver_drop(sender, destination, key).await;
        }
        let result = match result.await {
            Ok(result) => result?,
            Err(_) if deliveries.is_closed() => DriverAck::Dropped,
            Err(error) => return Err(error).context("driver dropped acknowledgement; retry destination"),
        };
        match result {
            DriverAck::Delivered => Ok(()),
            DriverAck::Dropped => self.driver_drop(sender, destination, key).await,
        }
    }

    async fn open_driver(&self, owner: &str, spawn: DriverSpawn) -> Result<()> {
        let _lifecycle = self.guard(&format!("lifecycle:{owner}")).await;
        let source = self.open_actor(owner).await?;
        let mut conn = source.conn.lock().await;
        let receipt = format!("driver_spawn:{}", spawn.id);
        if !actor::query(&conn, "SELECT value FROM meta WHERE key=?", [receipt.as_str()]).await?.rows.is_empty() {
            return Ok(());
        }
        if actor::status(&conn).await? == Status::Stopped {
            let tx = conn.transaction().await?;
            actor::set_meta(&tx, &receipt, "owner_stopped").await?;
            return self.commit_control(owner, tx).await;
        }
        let definition = self.registry.resolve_driver(&spawn.hash).await.with_context(|| format!("unknown driver hash {}", spawn.hash))?;
        ensure!(definition.hash() == spawn.hash, "driver hash {}: registry identity mismatch", spawn.hash);
        self.verify_cap_on(&conn, &spawn.owner, Rights::SEND, "driver owner").await?;
        let generation: i64 = actor::meta(&conn, "generation").await?.parse()?;
        // This receipt deliberately prevents automatic restoration on node restart,
        // even if a crash preceded the pump's delivered bit. Owner state reopens it.
        let tx = conn.transaction().await?;
        actor::set_meta(&tx, &receipt, &spawn.hash).await?;
        self.commit_control(owner, tx).await?;
        drop(conn);

        let (deliveries, incoming) = mpsc::channel(64);
        let (inject, mut injections) = mpsc::channel(64);
        let (finished, closed) = watch::channel(false);
        let completion = Completion(finished);
        let intentional = Arc::new(AtomicBool::new(false));
        let owner_epoch = spawn.owner.epoch;
        let cx = DriverContext { driver_id: spawn.id.clone(), handle: "root".into(), owner: spawn.owner.clone(), inject };
        let worker = tokio::spawn(async move {
            let _completion = completion;
            definition.run(cx, &spawn.init, incoming).await
        });
        self.drivers.running.lock().map_err(|_| anyhow::anyhow!("driver registry poisoned"))?.insert(
            spawn.id.clone(),
            Running { owner: owner.into(), deliveries, abort: worker.abort_handle(), closed, intentional: intentional.clone() },
        );
        let mut node = self.clone();
        node.driver_lifetime = None;
        node.background = None;
        let owner = owner.to_owned();
        // This broker is not aborted: a dropped injection caller still completes
        // its database transaction. It exits after worker completion and DOWN.
        tokio::spawn(async move {
            let mut worker = worker;
            let mut injecting = true;
            let outcome = loop {
                tokio::select! {
                    request = injections.recv(), if injecting => match request {
                        Some(request) => {
                            let result = node.inject_driver(&owner, generation, owner_epoch, &request).await;
                            let _ = request.ack.send(result);
                        }
                        None => injecting = false,
                    },
                    outcome = &mut worker => break outcome,
                }
            };
            while let Ok(request) = injections.try_recv() {
                let result = node.inject_driver(&owner, generation, owner_epoch, &request).await;
                let _ = request.ack.send(result);
            }
            // The worker is finished either way: this is the leaver for the `running`
            // entry. The owner's `driver_spawn:<id>` receipt, not this map, is what
            // stops a retried spawn row from opening a second resource.
            if let Ok(mut running) = node.drivers.running.lock() {
                running.remove(&spawn.id);
            }
            if intentional.load(Ordering::Acquire) {
                return;
            }
            let reason = match outcome {
                Ok(Ok(())) => "normal".to_owned(),
                Ok(Err(error)) => format!("{error:#}"),
                Err(error) => format!("driver task failed: {error}"),
            };
            loop {
                match node.driver_down(&owner, &spawn.id, generation, &reason).await {
                    Ok(()) => break,
                    Err(error) => {
                        if node.shipping.closed.load(Ordering::Acquire) || intentional.load(Ordering::Acquire) {
                            break;
                        }
                        eprintln!("driver {}: DOWN delivery failed: {error:#}", spawn.id);
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        });
        Ok(())
    }

    async fn inject_driver(&self, owner: &str, generation: i64, owner_epoch: u64, request: &Injection) -> Result<()> {
        // A pump may hold read admission while awaiting this driver. Do not queue
        // behind a close writer and deadlock that pump; refuse new ingress instead.
        let _admission = self.admission.clone().try_read_owned().context("node is closing; driver ingress refused")?;
        ensure!(!request.cap.target.starts_with("drv:"), "driver inject requires an actor capability");
        self.check_lease(owner)?;
        let source = self.open_actor(owner).await?;
        // Serializes against owner stop/reset, without admitting resource work to
        // an actor transaction. Release this before taking another actor's lock.
        let _lifecycle = self.guard(&format!("lifecycle:{owner}")).await;
        let epoch: u64 = {
            let conn = source.conn.lock().await;
            ensure!(actor::status(&conn).await? != Status::Stopped, "driver owner stopped");
            ensure!(actor::meta(&conn, "generation").await?.parse::<i64>()? == generation, "driver owner incarnation changed");
            let epoch = actor::meta(&conn, "capability_epoch").await?.parse()?;
            ensure!(epoch == owner_epoch, "driver owner capability epoch changed");
            epoch
        };
        let receiver = self.open_actor(&request.cap.target).await?;
        let mut conn = receiver.conn.lock().await;
        let tx = conn.transaction().await?;
        self.verify_cap_on(&tx, &request.cap, Rights::SEND, "driver inject").await?;
        let cap = self.mint_cap_at(&request.sender, epoch, format!("driver-handle:{}:{epoch}", request.sender).as_bytes());
        let cap = self.attenuate_verified(&cap, Rights::SEND)?;
        crate::capability::store_cap(&tx, &cap).await?;
        actor::inject(&tx, &request.key, &request.sender, &request.bytes).await?;
        self.check_lease(owner)?;
        self.commit_control(&request.cap.target, tx).await?;
        self.wake_actor(&request.cap.target)?;
        Ok(())
    }

    async fn driver_down(&self, owner: &str, id: &str, generation: i64, reason: &str) -> Result<()> {
        let _admission = self.admission.clone().try_read_owned().context("node is closing; driver DOWN deferred")?;
        let actor = self.open_actor(owner).await?;
        let mut conn = actor.conn.lock().await;
        let tx = conn.transaction().await?;
        let from = format!("drv:{id}:root");
        let reference = format!("spawn:{from}");
        let msg = serde_json::to_vec(&serde_json::json!({
            "type":"down", "ref":reference, "from":from, "child":from, "seq":-1, "reason":reason,
            "generation":generation, "event":format!("{from}:down"), "initiator":""
        }))?;
        actor::inject(&tx, &format!("down:{reference}"), &from, &msg).await?;
        actor::set_meta(&tx, &format!("driver_down:{id}"), reason).await?;
        self.commit_control(owner, tx).await?;
        self.wake_actor(owner)?;
        Ok(())
    }

    pub(crate) async fn close_drivers(&self, owner: Option<&str>, id: Option<&str>) -> Result<()> {
        let mut completions = Vec::new();
        {
            let running = self.drivers.running.lock().map_err(|_| anyhow::anyhow!("driver registry poisoned"))?;
            for (driver_id, driver) in running.iter() {
                if owner.is_some_and(|owner| owner != driver.owner) || id.is_some_and(|id| id != driver_id) {
                    continue;
                }
                driver.intentional.store(true, Ordering::Release);
                driver.abort.abort();
                completions.push(driver.closed.clone());
            }
        }
        // abort() lands at the worker's next await; a driver that never yields would
        // hang every closer, so the wait is bounded and the hang becomes a named error.
        for mut closed in completions {
            if !*closed.borrow() {
                tokio::time::timeout(CLOSE_TIMEOUT, closed.wait_for(|done| *done))
                    .await
                    .map_err(|_| anyhow::anyhow!("driver did not stop within {CLOSE_TIMEOUT:?}: Driver::run must yield at an await for cancellation"))?
                    .context("driver completion lost")?;
            }
        }
        Ok(())
    }
}
