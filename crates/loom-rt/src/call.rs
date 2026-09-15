//! Call-local root effects. The caller owns effect lifetime and persistence.
use super::{EffectContext, EffectOutput, Runtime, Value};
use tokio::sync::{mpsc, oneshot};

pub use loom_sandbox::{CallEffects, GuestFailure};

pub(super) fn wasm_error(error: wasmtime::Error) -> anyhow::Error {
    if error.is::<wasmtime::Trap>() {
        GuestFailure::new(format!("{error:#}")).into()
    } else {
        error.into()
    }
}

pub(super) struct Request {
    pub descriptor: Result<Value, GuestFailure>,
    pub reply: oneshot::Sender<anyhow::Result<EffectOutput>>,
}

impl Runtime {
    /// Read the optional pure `loom_schema` export without creating an actor.
    /// Registration validates the executable before publishing its hash.
    pub async fn definition_schema(&self, hash: &str) -> anyhow::Result<String> {
        let definition = self
            .inner
            .store
            .executable_definition(hash)?
            .ok_or_else(|| anyhow::anyhow!("definition {hash} not found"))?;
        if definition.lang.is_v8() {
            return Ok(self.javascript_program(hash).await?.schema().to_owned());
        }
        let call = self
            .core_execute(
                hash,
                "schema",
                &EffectContext::default(),
                true,
                super::sharedcore::Entry::Schema,
            )
            .await?;
        call.output
            .decode()?
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                GuestFailure::new(format!(
                    "definition {hash}: loom_schema must return SQL text"
                ))
                .into()
            })
    }

    /// Execute one definition with caller-owned root effects and persistence.
    /// Arguments follow
    /// the ordinary positional `call_def` ABI; outputs use canonical Loom CBOR.
    pub async fn call_with_effects(
        &self,
        hash: &str,
        args: Value,
        handler: &mut dyn CallEffects,
    ) -> anyhow::Result<Value> {
        let (sender, mut receiver) = mpsc::channel::<Request>(1);
        let effects = EffectContext {
            root: Some(sender),
            ..EffectContext::default()
        };
        let scope = format!("call:{}", uuid::Uuid::new_v4());
        let execution = self.call_scoped(hash, args, &scope, effects);
        tokio::pin!(execution);
        loop {
            tokio::select! {
                result = &mut execution => return result?.decode(),
                Some(request) = receiver.recv() => {
                    // Preserve the caller's typed error. Guest code cannot catch
                    // it and accidentally commit a partially failed transaction.
                    let output = handler.perform(request.descriptor?).await?;
                    let output = EffectOutput::value(&output)?;
                    let _ = request.reply.send(Ok(output));
                }
            }
        }
    }
}

pub(super) async fn dispatch(
    sender: &mpsc::Sender<Request>,
    descriptor: Result<Value, GuestFailure>,
) -> anyhow::Result<EffectOutput> {
    let (reply, receiver) = oneshot::channel();
    sender
        .send(Request { descriptor, reply })
        .await
        .map_err(|_| anyhow::anyhow!("call root effect receiver closed"))?;
    receiver
        .await
        .map_err(|_| anyhow::anyhow!("call root effect reply cancelled"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wasm_traps_are_guest_failures_and_host_io_preserves_its_type() {
        let trap = wasmtime::Error::new(wasmtime::Trap::UnreachableCodeReached);
        assert!(wasm_error(trap).is::<GuestFailure>());
        let host = std::io::Error::other("disk unavailable");
        let error = wasm_error(wasmtime::Error::from_anyhow(host.into()));
        assert!(error.is::<std::io::Error>());
        assert!(!error.is::<GuestFailure>());
    }

    #[test]
    fn guest_failure_survives_a_host_callback_error_round_trip() {
        let error = anyhow::Error::new(GuestFailure::new("unknown effect nope"));
        let error = wasm_error(wasmtime::Error::from_anyhow(error));
        assert!(error.is::<GuestFailure>());
        assert_eq!(error.to_string(), "unknown effect nope");
    }
}
