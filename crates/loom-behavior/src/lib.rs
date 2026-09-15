//! Execute stored Loom definitions inside a loom-actor message transaction.
#![forbid(unsafe_code)]

mod effects;
mod store_effects;
pub use store_effects::StoreEffects;
mod template;
mod wire;
pub use template::LoomTemplate;

use anyhow::{Context, Result};
use async_trait::async_trait;
use loom_actor::{Behavior, Ctx, Registry, Trap};
use loom_rt::{Runtime, WasmSandbox};
use loom_sandbox::{CallEffects, GuestFailure, Sandbox};
use loom_store::Store;
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
        Self::load(store, def_hash, runtime).await
    }

    pub fn from_sandbox(hash: String, schema: String, sandbox: Arc<dyn Sandbox>) -> Self {
        Self {
            hash,
            schema,
            sandbox,
            allowed_effects: None,
        }
    }

    async fn load(store: Store, def_hash: &str, runtime: Runtime) -> Result<Self> {
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
                && (definition.lang.is_v8()
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
            loom_proto::Lang::JavaScript | loom_proto::Lang::TypeScript => {
                let compiled = runtime.javascript_program(def_hash).await?;
                schema = compiled.schema().to_owned();
                sandbox = compiled;
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
    runtime: tokio::sync::OnceCell<Runtime>,
    resolved: tokio::sync::Mutex<std::collections::HashMap<String, Arc<dyn Behavior>>>,
}
impl StoreRegistry {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            runtime: Default::default(),
            resolved: Default::default(),
        }
    }
    /// The daemon already owns process recovery and engine pools. Resolutions
    /// must share that owner: constructing a new Runtime would mark live process
    /// sessions interrupted and create another set of execution workers.
    pub fn with_runtime(store: Store, runtime: Runtime) -> Self {
        Self {
            store,
            runtime: tokio::sync::OnceCell::new_with(Some(runtime)),
            resolved: Default::default(),
        }
    }
    async fn runtime(&self) -> Result<Runtime> {
        Ok(self
            .runtime
            .get_or_try_init(|| async { Runtime::new(self.store.clone()) })
            .await?
            .clone())
    }
}
#[async_trait]
impl Registry for StoreRegistry {
    async fn template(&self, reference: &str) -> Result<Arc<dyn loom_actor::Template>> {
        Ok(Arc::new(LoomTemplate::with_runtime(
            self.store.clone(),
            reference,
            self.runtime().await?,
        )?))
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
        let runtime = self.runtime().await?;
        let behavior: Arc<dyn Behavior> = Arc::new(
            LoomBehavior::load(self.store.clone(), &definition.hash, runtime)
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

    fn has_startup(&self) -> bool {
        self.sandbox.has_startup()
    }
    fn has_shutdown(&self) -> bool {
        self.sandbox.has_shutdown()
    }

    async fn startup(&self, cx: &mut Ctx<'_>) -> Result<(), Trap> {
        let mut effects = ActorEffects {
            cx,
            definition: &self.hash,
            allowed_effects: self.allowed_effects.as_deref(),
        };
        let result = self.sandbox.call_startup(&mut effects).await;
        effects.finish(result)
    }

    async fn terminate(&self, cx: &mut Ctx<'_>, reason: &str) -> Result<(), Trap> {
        let mut effects = ActorEffects {
            cx,
            definition: &self.hash,
            allowed_effects: self.allowed_effects.as_deref(),
        };
        let result = self.sandbox.call_shutdown(reason, &mut effects).await;
        effects.finish(result)
    }

    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let mut effects = ActorEffects {
            cx,
            definition: &self.hash,
            allowed_effects: self.allowed_effects.as_deref(),
        };
        let result = self.sandbox.call_message(msg, &mut effects).await;
        effects.finish(result)
    }
}

struct ActorEffects<'cx, 'db> {
    cx: &'cx mut Ctx<'db>,
    definition: &'cx str,
    allowed_effects: Option<&'cx [String]>,
}
impl ActorEffects<'_, '_> {
    fn finish(&mut self, result: Result<Value>) -> Result<(), Trap> {
        match result {
            Ok(_) => Ok(()),
            Err(error) => match error.downcast::<Trap>() {
                Ok(trap) => Err(trap),
                Err(error) if error.is::<GuestFailure>() => Err(Trap::new(format!(
                    "definition {}: {error:#}",
                    self.definition
                ))),
                Err(error) => Err(self
                    .cx
                    .runtime(format!("definition {}: {error:#}", self.definition))),
            },
        }
    }
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
