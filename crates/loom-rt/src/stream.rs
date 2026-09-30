//! Generators: an entry that yields many values before it returns.
//!
//! A guest calls `loom.yield_value` (the SDK's `loom::stream::emit`); the value goes into a
//! bounded channel and the guest does not run on until the consumer has room, so a slow
//! consumer stalls the producer instead of buffering a whole mesh. Dropping the [`CallStream`]
//! cancels the execution: the guest's next `emit` reports the consumer is gone. Only the root of
//! a stream can yield; an isolated callee has no stream (`EffectContext::delegated`).
use super::*;

/// Values a producer may run ahead of its consumer.
const WINDOW: usize = 4;

/// The values one entry yields, then its return value.
pub struct CallStream {
    items: tokio::sync::mpsc::Receiver<Vec<u8>>,
    task: Option<tokio::task::JoinHandle<Result<Value>>>,
}

impl CallStream {
    /// The next yielded value, decoded; `None` once the entry has returned (call
    /// [`Self::finish`] for its result or its error).
    pub async fn next(&mut self) -> Option<Result<Value>> {
        let bytes = self.items.recv().await?;
        Some(loom_proto::decode_host(&bytes).map_err(anyhow::Error::msg))
    }

    /// The next yielded value as its encoded bytes (DAG-CBOR), without decoding.
    pub async fn next_bytes(&mut self) -> Option<Vec<u8>> {
        self.items.recv().await
    }

    /// Wait for the entry to return and give its result. Values not yet taken are discarded.
    pub async fn finish(mut self) -> Result<Value> {
        self.items.close();
        while self.items.recv().await.is_some() {}
        let task = self.task.take().expect("finish consumes the stream once");
        task.await.map_err(|error| anyhow::anyhow!("stream task failed: {error}"))?
    }
}

impl Drop for CallStream {
    fn drop(&mut self) {
        // Closing the receiver makes the guest's next `emit` fail; aborting the task stops a
        // guest that is busy between emits at its next suspension point.
        self.items.close();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Runtime {
    /// Run `entry` of `hash` as a generator: see the module documentation.
    pub fn call_stream(&self, hash: &str, entry: Option<&str>, args: Value) -> CallStream {
        let (sender, receiver) = tokio::sync::mpsc::channel(WINDOW);
        let runtime = self.clone();
        let (hash, entry) = (hash.to_owned(), entry.map(str::to_owned));
        let task = tokio::spawn(async move {
            let scope = format!("call:{}", uuid::Uuid::new_v4());
            let execution = trace::ExecutionTrace::fresh(&scope);
            Ok(runtime
                .call_traced_entry_streaming(
                    &hash,
                    entry.as_deref(),
                    args,
                    &scope,
                    execution,
                    Some(sender),
                )
                .await?
                .value)
        });
        CallStream {
            items: receiver,
            task: Some(task),
        }
    }
}
