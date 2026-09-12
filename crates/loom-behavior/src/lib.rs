//! Execute stored Loom definitions inside a loom-actor message transaction.
#![forbid(unsafe_code)]

mod effects;
mod wire;

use anyhow::{Context, Result};
use async_trait::async_trait;
use loom_actor::{Behavior, Ctx, Registry, Trap};
use loom_rt::{CallEffects, GuestFailure, Runtime};
use loom_store::Store;
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc};

/// A definition is stateless between calls; durable state belongs to `Ctx::sql`.
pub struct LoomBehavior {
    hash: String,
    schema: String,
    runtime: Runtime,
}

impl LoomBehavior {
    pub async fn new(store: Store, def_hash: &str) -> Result<Self> {
        let definition = store
            .executable_definition(def_hash)?
            .with_context(|| format!("definition {def_hash} not found"))?;
        let runtime = Runtime::new(store)?;
        let schema = runtime
            .definition_schema(def_hash)
            .await
            .with_context(|| format!("definition {def_hash}: schema export"))?;
        Ok(Self {
            hash: definition.hash,
            schema,
            runtime,
        })
    }
}

/// Register before constructing `Node`, using the exact `defs.hash` identity.
pub async fn register(
    registry: &mut Registry,
    store: Store,
    def_hash: &str,
) -> Result<Arc<LoomBehavior>> {
    let behavior = Arc::new(LoomBehavior::new(store, def_hash).await?);
    registry.insert(behavior.hash.clone(), behavior.clone());
    Ok(behavior)
}

#[async_trait]
impl Behavior for LoomBehavior {
    fn hash(&self) -> &str {
        &self.hash
    }
    fn schema(&self) -> &str {
        &self.schema
    }

    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        // One positional Vec<u8> argument preserves arbitrary inbox bytes.
        let args = serde_json::json!([msg]);
        let mut effects = ActorEffects {
            cx,
            definition: &self.hash,
        };
        match self
            .runtime
            .call_with_effects(&self.hash, args, &mut effects)
            .await
        {
            Ok(_) => Ok(()),
            Err(error) => match error.downcast::<Trap>() {
                Ok(trap) => Err(trap),
                Err(error) if error.is::<GuestFailure>() => {
                    Err(Trap::new(format!("definition {}: {error:#}", self.hash)))
                }
                Err(error) => Err(effects
                    .cx
                    .runtime(format!("definition {}: {error:#}", self.hash))),
            },
        }
    }
}

struct ActorEffects<'cx, 'db> {
    cx: &'cx mut Ctx<'db>,
    definition: &'cx str,
}
impl CallEffects for ActorEffects<'_, '_> {
    fn perform(
        &mut self,
        descriptor: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + '_>> {
        Box::pin(async move {
            effects::dispatch(self.cx, self.definition, descriptor)
                .await
                .map_err(anyhow::Error::new)
        })
    }
}
