//! Pure Value-to-Value calls for native view behaviors; no second guest scheduler.
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use loom_actor::{Ctx, Template, Trap};
use loom_proto::ValueShape;
use loom_rt::{CallEffects, GuestFailure, Runtime};
use loom_store::Store;
use serde_json::Value;
use std::{future::Future, pin::Pin};

pub struct LoomTemplate {
    hash: String,
    runtime: Runtime,
}

impl LoomTemplate {
    pub fn new(store: Store, reference: &str) -> Result<Self> {
        let definition = store
            .resolve(reference)?
            .with_context(|| format!("view-v1 template {reference:?} not found"))?;
        let definition = store
            .executable_definition(&definition.hash)?
            .with_context(|| format!("view-v1 template {reference:?} is not executable"))?;
        let [entry] = definition.sig.exports.as_slice() else {
            anyhow::bail!(
                "view-v1 template {}: expected exactly one render entry",
                definition.hash
            );
        };
        ensure!(
            entry.name == "render"
                && entry.params.len() == 1
                && matches!(&entry.params[0].shape, ValueShape::Value)
                && matches!(&entry.returns, ValueShape::Value),
            "view-v1 template {}: expected render(row: Value) -> Value",
            definition.hash
        );
        for row in [&definition.sig.effects, &entry.effects] {
            if let Some(effect) = row.labels.first() {
                anyhow::bail!(
                    "view-v1 template {}: forbidden effect {effect}",
                    definition.hash
                );
            }
            ensure!(
                !row.unknown,
                "view-v1 template {}: effect row unknown flag is set",
                definition.hash
            );
        }
        Ok(Self {
            hash: definition.hash,
            runtime: Runtime::new(store)?,
        })
    }
}

#[async_trait]
impl Template for LoomTemplate {
    fn hash(&self) -> &str {
        &self.hash
    }

    fn effects(&self) -> &[String] {
        &[]
    }

    async fn render(&self, cx: &mut Ctx<'_>, row: Value) -> Result<Value, Trap> {
        let mut effects = PureEffects { hash: &self.hash };
        match self
            .runtime
            .call_with_effects(&self.hash, serde_json::json!([row]), &mut effects)
            .await
        {
            Ok(tree) => Ok(tree),
            Err(error) => match error.downcast::<Trap>() {
                Ok(trap) => Err(trap),
                Err(error) if error.is::<GuestFailure>() => {
                    Err(Trap::new(format!("template {}: {error:#}", self.hash)))
                }
                Err(error) => Err(cx.runtime(format!("template {}: {error:#}", self.hash))),
            },
        }
    }
}

struct PureEffects<'a> {
    hash: &'a str,
}

impl CallEffects for PureEffects<'_> {
    fn perform(
        &mut self,
        descriptor: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + '_>> {
        Box::pin(async move {
            let effect = descriptor
                .get("op")
                .and_then(Value::as_str)
                .unwrap_or("<missing op>");
            Err(Trap::new(format!(
                "view-v1 template {}: forbidden effect {effect}",
                self.hash
            ))
            .into())
        })
    }
}
