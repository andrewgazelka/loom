//! Explicit native registrations composed with a stored-definition registry.
use crate::{Behavior, Driver, Registry, Template, builtin::BehaviorInfo};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use std::{collections::BTreeMap, sync::Arc};

pub struct CompositeRegistry {
    base: Arc<dyn Registry>,
    behaviors: BTreeMap<String, Arc<dyn Behavior>>,
    drivers: BTreeMap<String, Arc<dyn Driver>>,
    processes: BTreeMap<String, String>,
}

impl CompositeRegistry {
    pub fn new(base: Arc<dyn Registry>, behaviors: Vec<Arc<dyn Behavior>>, drivers: Vec<Arc<dyn Driver>>) -> Result<Self> {
        let mut registry = Self { base, behaviors: BTreeMap::new(), drivers: BTreeMap::new(), processes: BTreeMap::new() };
        for behavior in behaviors {
            let hash = behavior.hash().to_owned();
            ensure!(!hash.is_empty(), "native behavior identity is empty");
            ensure!(registry.behaviors.insert(hash.clone(), behavior).is_none(), "duplicate native behavior {hash}");
        }
        for driver in drivers {
            let hash = driver.hash().to_owned();
            ensure!(!hash.is_empty(), "native driver identity is empty");
            ensure!(registry.drivers.insert(hash.clone(), driver).is_none(), "duplicate native driver {hash}");
        }
        Ok(registry)
    }
    /// Names are tenant-local; durable driver spawns always pin the resolved hash.
    pub fn with_processes(mut self, processes: BTreeMap<String, String>) -> Result<Self> {
        for (name, hash) in &processes {
            ensure!(!name.is_empty(), "process preset name is empty");
            ensure!(self.drivers.contains_key(hash), "process preset {name} names unregistered driver {hash}");
        }
        self.processes = processes;
        Ok(self)
    }
}

#[async_trait]
impl Registry for CompositeRegistry {
    async fn resolve(&self, reference: &str) -> Result<Arc<dyn Behavior>> {
        if let Some(behavior) = self.behaviors.get(reference) {
            return Ok(behavior.clone());
        }
        self.base.resolve(reference).await
    }

    async fn behaviors(&self) -> Result<Vec<BehaviorInfo>> {
        let mut behaviors = self.base.behaviors().await?;
        for behavior in self.behaviors.values() {
            ensure!(
                !behaviors.iter().any(|entry| entry.hash == behavior.hash()),
                "native behavior identity collides with stored definition {}",
                behavior.hash()
            );
            behaviors.push(BehaviorInfo { hash: behavior.hash().into(), description: behavior.description().into() });
        }
        Ok(behaviors)
    }

    async fn template(&self, reference: &str) -> Result<Arc<dyn Template>> {
        self.base.template(reference).await
    }

    async fn resolve_process(&self, name: &str) -> Result<String> {
        self.processes.get(name).cloned().ok_or_else(|| anyhow::anyhow!("unknown process preset {name}"))
    }

    async fn resolve_driver(&self, hash: &str) -> Result<Arc<dyn Driver>> {
        if let Some(driver) = self.drivers.get(hash) {
            return Ok(driver.clone());
        }
        self.base.resolve_driver(hash).await
    }
}
