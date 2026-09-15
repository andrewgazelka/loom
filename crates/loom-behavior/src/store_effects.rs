//! Tenant storage stays behind the actor's existing recorded effect boundary.
use async_trait::async_trait;
use loom_actor::{EffectError, EffectHandler, EffectKey};
use loom_store::Store;
use std::sync::Arc;

pub struct StoreEffects {
    store: Store,
    fallback: Arc<dyn EffectHandler>,
}
impl StoreEffects {
    pub fn new(store: Store, fallback: Arc<dyn EffectHandler>) -> Self {
        Self { store, fallback }
    }
}
#[async_trait]
impl EffectHandler for StoreEffects {
    async fn call(
        &self,
        key: &EffectKey,
        kind: &str,
        request: &[u8],
    ) -> Result<Vec<u8>, EffectError> {
        if !matches!(
            kind,
            "cas.put" | "cas.get" | "cas.put_bytes" | "cas.get_bytes"
        ) {
            return self.fallback.call(key, kind, request).await;
        }
        let args = serde_json::from_slice(request)
            .map_err(|error| EffectError::Deterministic(error.into()))?;
        let result = self.store.guest_cas_effect(kind, args).map_err(|error| {
            if error.is::<loom_store::DatabaseError>() || error.is::<std::io::Error>() {
                EffectError::Environmental(error)
            } else {
                EffectError::Deterministic(error)
            }
        })?;
        if matches!(kind, "cas.put" | "cas.put_bytes") {
            // The actor database may commit immediately after this reply. Its
            // reference must never become durable before the separate CAS WAL.
            self.store.flush().map_err(EffectError::Environmental)?;
        }
        serde_json::to_vec(&result).map_err(|error| EffectError::Deterministic(error.into()))
    }
}
