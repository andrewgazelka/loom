//! Execute stored Loom definitions inside a loom-actor message transaction.
#![forbid(unsafe_code)]

mod effects;
mod template;
mod wire;
pub use template::LoomTemplate;

use anyhow::{Context, Result};
use async_trait::async_trait;
use loom_actor::{Behavior, Ctx, Registry, Trap};
use loom_rt::{Runtime, WasmSandbox};
use loom_sandbox::{CallEffects, GuestFailure, Sandbox};
use loom_store::Store;
use loom_v8::V8Engine;
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc};

/// A definition is stateless between calls; durable state belongs to `Ctx::sql`.
pub struct LoomBehavior {
    hash: String,
    schema: String,
    sandbox: Arc<dyn Sandbox>,
    allowed_effects: Option<Vec<String>>,
}

impl LoomBehavior {
    pub async fn new(store: Store, def_hash: &str) -> Result<Self> {
        let runtime = Runtime::new(store.clone())?;
        Self::load(store, def_hash, runtime, None).await
    }

    pub fn from_sandbox(hash: String, schema: String, sandbox: Arc<dyn Sandbox>) -> Self {
        Self {
            hash,
            schema,
            sandbox,
            allowed_effects: None,
        }
    }

    async fn load(
        store: Store,
        def_hash: &str,
        runtime: Runtime,
        v8: Option<Arc<V8Engine>>,
    ) -> Result<Self> {
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
                && (definition.lang == loom_proto::Lang::JavaScript
                    || matches!(
                        &entry.params[0].shape,
                        loom_proto::ValueShape::Array { items } if matches!(items.as_ref(), loom_proto::ValueShape::Number)
                    )),
            "actor definition {def_hash}: entry {} must accept one byte-array message",
            entry.name
        );
        let sandbox: Arc<dyn Sandbox>;
        let schema;
        match definition.lang {
            loom_proto::Lang::Rust => {
                schema = runtime
                    .definition_schema(def_hash)
                    .await
                    .with_context(|| format!("definition {def_hash}: LOOM_SCHEMA export"))?;
                sandbox = Arc::new(WasmSandbox::new(runtime, def_hash.to_owned()));
            }
            loom_proto::Lang::JavaScript => {
                let engine = match v8 {
                    Some(engine) => engine,
                    None => runtime.v8_engine()?,
                };
                let source = store.javascript_source(def_hash, loom_v8::ABI_VERSION)?;
                let compiled = engine.compile(&source).await?;
                schema = compiled.schema().to_owned();
                sandbox = Arc::new(compiled);
            }
        }
        Ok(Self {
            hash: definition.hash,
            schema,
            sandbox,
            allowed_effects: definition.allowed_effects,
        })
    }
}

/// Resolves the current name binding on every lookup; actors pin the returned hash.
pub struct StoreRegistry {
    store: Store,
    v8: tokio::sync::OnceCell<Arc<V8Engine>>,
    resolved: tokio::sync::Mutex<std::collections::HashMap<String, Arc<dyn Behavior>>>,
}
impl StoreRegistry {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            v8: Default::default(),
            resolved: Default::default(),
        }
    }
    pub fn with_v8(store: Store, engine: Arc<V8Engine>) -> Self {
        Self {
            store,
            v8: tokio::sync::OnceCell::new_with(Some(engine)),
            resolved: Default::default(),
        }
    }
}
#[async_trait]
impl Registry for StoreRegistry {
    async fn template(&self, reference: &str) -> Result<Arc<dyn loom_actor::Template>> {
        Ok(Arc::new(LoomTemplate::new(self.store.clone(), reference)?))
    }

    async fn resolve(&self, reference: &str) -> Result<Arc<dyn Behavior>> {
        let definition = self
            .store
            .resolve(reference)?
            .with_context(|| format!("actor definition {reference:?} not found"))?;
        let mut resolved = self.resolved.lock().await;
        if let Some(behavior) = resolved.get(&definition.hash) {
            return Ok(behavior.clone());
        }
        let runtime = Runtime::new(self.store.clone())?;
        let v8 = if definition.lang == loom_proto::Lang::JavaScript {
            Some(
                self.v8
                    .get_or_try_init(|| async { runtime.v8_engine() })
                    .await?
                    .clone(),
            )
        } else {
            None
        };
        let behavior: Arc<dyn Behavior> = Arc::new(
            LoomBehavior::load(self.store.clone(), &definition.hash, runtime, v8)
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
            allowed_effects: self.allowed_effects.as_deref(),
        };
        match self.sandbox.call(args, &mut effects).await {
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
    allowed_effects: Option<&'cx [String]>,
}
impl CallEffects for ActorEffects<'_, '_> {
    fn perform(
        &mut self,
        descriptor: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + '_>> {
        Box::pin(async move {
            if let Some(allowed) = self.allowed_effects {
                let op = descriptor
                    .get("op")
                    .and_then(Value::as_str)
                    .unwrap_or("<missing op>");
                anyhow::ensure!(
                    allowed.iter().any(|effect| effect == op),
                    Trap::new(format!(
                        "effect {op} is not allowed for definition {}",
                        self.definition
                    ))
                );
            }
            effects::dispatch(self.cx, self.definition, descriptor)
                .await
                .map_err(anyhow::Error::new)
        })
    }
}
