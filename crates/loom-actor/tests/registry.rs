use async_trait::async_trait;
use loom_actor::{Behavior, builtin::BehaviorInfo};
use std::{collections::HashMap, sync::Arc};

pub struct Registry {
    values: HashMap<String, Arc<dyn Behavior>>,
    drivers: HashMap<String, Arc<dyn loom_actor::Driver>>,
}

impl Registry {
    pub fn new() -> Self {
        let mut values = HashMap::new();
        for behavior in [
            Arc::new(loom_actor::builtin::Counter::plain()) as Arc<dyn Behavior>,
            Arc::new(loom_actor::builtin::Forwarder { trap: false }),
            Arc::new(loom_actor::builtin::Echo),
        ] {
            values.insert(behavior.hash().to_owned(), behavior);
        }
        Self { values, drivers: HashMap::new() }
    }

    #[allow(dead_code)] // Shared fixture also serves non-driver test targets.
    pub fn insert_driver(&mut self, driver: Arc<dyn loom_actor::Driver>) {
        self.drivers.insert(driver.hash().to_owned(), driver);
    }

    pub fn insert(&mut self, hash: String, behavior: Arc<dyn Behavior>) {
        assert_eq!(hash, behavior.hash());
        self.values.insert(hash, behavior);
    }
}

#[async_trait]
impl loom_actor::Registry for Registry {
    async fn resolve(&self, reference: &str) -> anyhow::Result<Arc<dyn Behavior>> {
        self.values.get(reference).cloned().ok_or_else(|| anyhow::anyhow!("unknown test behavior {reference}"))
    }

    async fn resolve_driver(&self, hash: &str) -> anyhow::Result<Arc<dyn loom_actor::Driver>> {
        self.drivers.get(hash).cloned().ok_or_else(|| anyhow::anyhow!("unknown driver hash {hash}"))
    }

    async fn behaviors(&self) -> anyhow::Result<Vec<BehaviorInfo>> {
        Ok(self
            .values
            .iter()
            .map(|(hash, behavior)| BehaviorInfo { hash: hash.clone(), description: behavior.description().to_owned() })
            .collect())
    }
}
