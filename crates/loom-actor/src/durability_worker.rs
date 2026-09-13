//! Independent shipping and per-actor renewal workers share an aborting supervisor.
use crate::Node;
use anyhow::Result;
use std::sync::{Arc, Mutex, atomic::Ordering};

#[derive(Clone, Debug)]
pub struct ShippingFailure {
    pub actor_id: String,
    pub error: String,
}
pub(crate) struct Background {
    stop: Arc<tokio::sync::Notify>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Drop for Background {
    fn drop(&mut self) {
        if let Ok(task) = self.task.get_mut()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }
}
impl Node {
    pub(crate) fn start_shipper(&mut self) {
        if self.remote.is_none() {
            return;
        }
        let node = self.clone(); // No Background in this clone: its owner can drop.
        let stop = Arc::new(tokio::sync::Notify::new());
        let signal = stop.clone();
        let task = tokio::spawn(async move {
            let mut workers = tokio::task::JoinSet::new();
            let worker_stop = Arc::new(tokio::sync::Notify::new());
            let renewal = node.config.lease_ttl / 3;
            let mut discover = tokio::time::interval(renewal.min(node.config.ship_interval));
            let mut membership = tokio::time::interval(renewal);
            let mut renewing = std::collections::BTreeSet::new();
            loop {
                tokio::select! {
                    _ = signal.notified() => break,
                    _ = membership.tick() => {
                        if let Err(error) = node.renew_node().await {
                            node.record_shipping_failure("<node>", &error);
                        }
                    }
                    _ = discover.tick() => {
                        let ids = node.shipping.actors.lock().expect("shipping state poisoned").keys().cloned().collect::<Vec<_>>();
                        for id in ids {
                            if !renewing.insert(id.clone()) { continue; }
                            let shipper = node.clone();
                            let shipping_id = id.clone();
                            let shipping_stop = worker_stop.clone();
                            workers.spawn(async move {
                                let mut ship = tokio::time::interval(shipper.config.ship_interval);
                                loop {
                                    if shipper.shipping.closed.load(Ordering::Acquire) { break; }
                                    tokio::select! {
                                        _ = shipping_stop.notified() => break,
                                        _ = ship.tick() => {},
                                    }
                                    if shipper.connections.lock().await.contains_key(&shipping_id) && shipper.shipping.get(&shipping_id).is_ok_and(|state| state.baseline.is_some()) {
                                        let _ = shipper.ship_inner(&shipping_id).await;
                                    }
                                }
                            });
                            let owner = node.clone();
                            let renewal_stop = worker_stop.clone();
                            workers.spawn(async move {
                                let mut tick = tokio::time::interval(renewal);
                                loop {
                                    if owner.shipping.closed.load(Ordering::Acquire) { break; }
                                    tokio::select! {
                                        _ = renewal_stop.notified() => break,
                                        _ = tick.tick() => {},
                                    }
                                    if owner.shipping.get(&id).is_ok() {
                                        let _ = owner.renew_actor(&id).await;
                                    }
                                }
                            });
                        }
                    }
                    Some(_) = workers.join_next() => {}
                }
            }
            worker_stop.notify_waiters();
            while workers.join_next().await.is_some() {}
        });
        self.background = Some(Arc::new(Background { stop, task: Mutex::new(Some(task)) }));
    }
    pub(crate) fn record_shipping_failure(&self, id: &str, error: &anyhow::Error) {
        eprintln!("actor {id}: shipping failed: {error:#}");
        let mut failures = self.shipping.failures.lock().expect("shipping failures mutex poisoned");
        if let Some(previous) = failures.iter_mut().find(|failure| failure.actor_id == id) {
            previous.error = format!("{error:#}");
        } else {
            failures.push(ShippingFailure { actor_id: id.into(), error: format!("{error:#}") });
        }
    }
    pub fn shipping_failures(&self) -> Vec<ShippingFailure> {
        self.shipping.failures.lock().expect("shipping failures mutex poisoned").clone()
    }
    async fn renew_actor(&self, id: &str) -> Result<()> {
        let Some(store) = &self.remote else {
            return Ok(());
        };
        if let Err(error) = store.renew(id).await {
            self.record_shipping_failure(id, &error);
            if let Some(signal) = self.tasks.lock().await.get(id) {
                signal.notify_one();
            }
            let connection = self.connections.lock().await.get(id).cloned();
            if let Some(connection) = connection {
                let mut conn = connection.lock().await;
                if self.path(id).exists() {
                    self.archive_stale(id, &mut conn).await?;
                }
            }
            return Err(error);
        }
        Ok(())
    }
    pub async fn renew_leases(&self) -> Result<()> {
        let _admission = self.admit().await?;
        self.renew_node().await?;
        let actors = self.connections.lock().await.keys().cloned().collect::<Vec<_>>();
        let mut workers = tokio::task::JoinSet::new();
        for id in actors {
            let node = self.clone();
            workers.spawn(async move { node.renew_actor(&id).await });
        }
        let mut failure = None;
        while let Some(result) = workers.join_next().await {
            if let Err(error) = result.map_err(anyhow::Error::from).and_then(|result| result) {
                failure = Some(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    /// Flush while renewal remains active; a failed flush leaves the node usable.
    /// Once closed, lease-release failures are retried by calling close again.
    pub async fn close(&self) -> Result<()> {
        let _admission = self.admission.write().await;
        let _run = self.run_gate.lock().await;
        let actors = self.connections.lock().await.keys().cloned().collect::<Vec<_>>();
        if !self.shipping.closed.load(Ordering::Acquire) {
            for id in &actors {
                self.ship_inner(id).await?;
            }
            self.shipping.closed.store(true, Ordering::Release);
            self.wake.notify_waiters();
        }
        if let Some(background) = &self.background {
            background.stop.notify_one();
            let task = background.task.lock().map_err(|_| anyhow::anyhow!("shipper task mutex poisoned"))?.take();
            if let Some(task) = task {
                task.await?;
            }
        }
        if let Some(store) = &self.remote {
            for id in actors {
                if self.shipping.get(&id).is_ok() {
                    store.release(&id).await?;
                    self.shipping.actors.lock().map_err(|_| anyhow::anyhow!("shipping state poisoned"))?.remove(&id);
                }
            }
        }
        Ok(())
    }
}
