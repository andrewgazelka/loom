use loom_actor::Node;

pub struct ClusterWorker {
    // finish (or Drop on early server failure) signals the worker out of idle waits.
    stop: tokio::sync::watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl ClusterWorker {
    pub fn start(node: &Node) -> Option<Self> {
        node.identity()?;
        let channel = tokio::sync::watch::channel(false);
        let stop = channel.0;
        let receiver = channel.1;
        let node = node.clone();
        Some(Self {
            stop,
            task: Some(tokio::spawn(
                async move { node.run_service(receiver).await },
            )),
        })
    }

    pub async fn finish(mut self, node: &Node) -> anyhow::Result<()> {
        self.stop.send_replace(true);
        let drained = match self.task.take() {
            Some(task) => task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result),
            None => Ok(()),
        };
        let closed = node.close().await;
        drained?;
        closed
    }
}

impl Drop for ClusterWorker {
    fn drop(&mut self) {
        // Never abort an in-flight transaction. The cooperative scheduler drains it.
        self.stop.send_replace(true);
    }
}
