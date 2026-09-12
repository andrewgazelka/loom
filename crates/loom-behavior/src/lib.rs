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
        let [entry] = definition.sig.exports.as_slice() else {
            anyhow::bail!(
                "actor definition {def_hash}: expected exactly one entry; candidates: {}",
                definition
                    .sig
                    .exports
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        };
        anyhow::ensure!(
            entry.params.len() == 1
                && matches!(
                    &entry.params[0].shape,
                    loom_proto::ValueShape::Array { items } if matches!(items.as_ref(), loom_proto::ValueShape::Number)
                ),
            "actor definition {def_hash}: entry {} must accept one byte-array message",
            entry.name
        );
        let runtime = Runtime::new(store)?;
        // The build exports the evaluated root LOOM_SCHEMA through loom_schema.
        let schema = runtime
            .definition_schema(def_hash)
            .await
            .with_context(|| format!("definition {def_hash}: LOOM_SCHEMA export"))?;
        Ok(Self {
            hash: definition.hash,
            schema,
            runtime,
        })
    }
}

/// Resolves the current name binding on every lookup; actors pin the returned hash.
pub struct StoreRegistry {
    store: Store,
    resolved: tokio::sync::Mutex<std::collections::HashMap<String, Arc<dyn Behavior>>>,
}
impl StoreRegistry {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            resolved: Default::default(),
        }
    }
}
#[async_trait]
impl Registry for StoreRegistry {
    async fn resolve(&self, reference: &str) -> Result<Arc<dyn Behavior>> {
        let definition = self
            .store
            .resolve(reference)?
            .with_context(|| format!("actor definition {reference:?} not found"))?;
        let mut resolved = self.resolved.lock().await;
        if let Some(behavior) = resolved.get(&definition.hash) {
            return Ok(behavior.clone());
        }
        let behavior: Arc<dyn Behavior> = Arc::new(
            LoomBehavior::new(self.store.clone(), &definition.hash)
                .await
                .with_context(|| format!("actor definition {reference:?}"))?,
        );
        resolved.insert(definition.hash, behavior.clone());
        Ok(behavior)
    }
    async fn behaviors(&self) -> Result<Vec<loom_actor::builtin::BehaviorInfo>> {
        Ok(self
            .store
            .definitions()?
            .into_iter()
            .filter(|definition| definition.component_hash.is_some())
            .map(|definition| loom_actor::builtin::BehaviorInfo {
                hash: definition.hash,
                description: "Stored Loom definition.".into(),
            })
            .collect())
    }
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
