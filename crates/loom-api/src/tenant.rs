//! Authenticated tenant selection happens before names, IDs, or CAS are resolved.
use crate::{Access, Authorizer, Service};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, sync::Arc};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TenantId(String);
impl TenantId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        ensure!(
            !value.is_empty()
                && value.len() <= 64
                && value.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'-'
                    || b == b'_'),
            "tenant must contain 1..64 lowercase ASCII letters, digits, hyphens, or underscores"
        );
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Default for TenantId {
    fn default() -> Self {
        Self("default".into())
    }
}
impl TryFrom<String> for TenantId {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}
impl From<TenantId> for String {
    fn from(value: TenantId) -> Self {
        value.0
    }
}
impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone)]
pub struct ServiceDirectory {
    services: Arc<BTreeMap<TenantId, Arc<Service>>>,
}
impl ServiceDirectory {
    pub fn new(services: impl IntoIterator<Item = Arc<Service>>) -> Result<Self> {
        let mut values: BTreeMap<TenantId, Arc<Service>> = BTreeMap::new();
        for service in services {
            for existing in values.values() {
                ensure!(
                    !service.store.shares_storage(&existing.store)?
                        && !service.runtime.shares_resources(&existing.runtime),
                    "tenant services must own separate storage and runtimes"
                );
                if let (Some(left), Some(right)) = (&service.actors, &existing.actors) {
                    ensure!(
                        !left.node.shares_storage(&right.node)?,
                        "tenant services share actor storage"
                    );
                }
                if let (Some(left), Some(right)) = (&service.websockets, &existing.websockets) {
                    ensure!(
                        !left.shares_resources(right),
                        "tenant services share WebSocket resources"
                    );
                }
                if let (Some(left), Some(right)) =
                    (&service.native_registry, &existing.native_registry)
                {
                    ensure!(
                        !Arc::ptr_eq(left, right),
                        "tenant services share a native registry"
                    );
                }
                let cache = canonical_location(service.build_directory())?;
                let existing_cache = canonical_location(existing.build_directory())?;
                ensure!(
                    !cache.starts_with(&existing_cache) && !existing_cache.starts_with(&cache),
                    "tenant services share mutable build storage"
                );
            }
            ensure!(
                values.insert(service.tenant().clone(), service).is_none(),
                "duplicate tenant service"
            );
        }
        ensure!(
            !values.is_empty(),
            "at least one tenant service is required"
        );
        Ok(Self {
            services: Arc::new(values),
        })
    }
    pub fn get(&self, tenant: &TenantId) -> Result<Arc<Service>> {
        self.services
            .get(tenant)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("tenant service unavailable"))
    }
    pub fn scoped(&self, access: Access) -> Result<Service> {
        self.get(access.tenant())?.scoped(access)
    }
    pub fn services(&self) -> impl Iterator<Item = &Arc<Service>> {
        self.services.values()
    }
    pub fn authorizer(&self, mut authorizer: Authorizer) -> Authorizer {
        for service in self.services.values() {
            if let Some(actors) = &service.actors {
                authorizer = authorizer
                    .with_tenant_ingress(service.tenant().clone(), actors.node.ingress_bearer());
            }
        }
        authorizer
    }
}
impl From<Arc<Service>> for ServiceDirectory {
    fn from(service: Arc<Service>) -> Self {
        Self {
            services: Arc::new(BTreeMap::from([(service.tenant().clone(), service)])),
        }
    }
}

#[derive(Clone)]
pub(crate) struct TenantService {
    pub(crate) service: Arc<Service>,
}

// Cache directories can be configured before creation. Resolve their nearest
// existing ancestor so symlink aliases cannot bypass tenant resource checks.
fn canonical_location(path: &std::path::Path) -> Result<std::path::PathBuf> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let absolute = std::path::absolute(path)?;
    let parent = absolute
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid resource path"))?;
    let name = absolute
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("invalid resource path"))?;
    Ok(canonical_location(parent)?.join(name))
}
