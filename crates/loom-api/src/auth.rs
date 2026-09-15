use crate::TenantId;
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{collections::BTreeSet, sync::Arc};
use subtle::ConstantTimeEq;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Read,
    Execute,
    Define,
    Admin,
    Host,
}
#[derive(Clone, Default)]
pub struct Access {
    scopes: BTreeSet<Scope>,
    tenant: TenantId,
}
impl Access {
    pub fn owner() -> Self {
        Self {
            tenant: TenantId::default(),
            scopes: [
                Scope::Read,
                Scope::Execute,
                Scope::Define,
                Scope::Admin,
                Scope::Host,
            ]
            .into_iter()
            .collect(),
        }
    }
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }
    pub fn for_tenant(mut self, tenant: TenantId) -> Self {
        if tenant != TenantId::default() {
            self.scopes.remove(&Scope::Host);
        }
        self.tenant = tenant;
        self
    }
    pub fn allows(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }
    pub fn require(&self, scope: Scope) -> Result<()> {
        if self.allows(scope) {
            Ok(())
        } else {
            Err(ScopeDenied { scope }.into())
        }
    }
}
#[derive(Debug)]
pub struct ScopeDenied {
    scope: Scope,
}
impl std::fmt::Display for ScopeDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} scope required", self.scope)
    }
}
impl std::error::Error for ScopeDenied {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenConfig {
    pub token: String,
    #[serde(default)]
    pub tenant: TenantId,
    pub scopes: BTreeSet<Scope>,
}
#[derive(Clone)]
struct TenantIngress {
    tenant: TenantId,
    bearer: String,
}
#[derive(Clone)]
pub struct Authorizer {
    tokens: Arc<Vec<TokenConfig>>,
    ingress: Arc<Vec<TenantIngress>>,
    pub(crate) public: bool,
}
impl Authorizer {
    pub fn single(token: String) -> Result<Self> {
        Self::new(vec![TokenConfig {
            token,
            tenant: TenantId::default(),
            scopes: Access::owner().scopes,
        }])
    }
    pub fn new(tokens: Vec<TokenConfig>) -> Result<Self> {
        ensure!(!tokens.is_empty(), "at least one token is required");
        let mut seen = BTreeSet::new();
        for entry in &tokens {
            ensure!(
                entry.tenant == TenantId::default() || !entry.scopes.contains(&Scope::Host),
                "host authority can only belong to the default operator tenant"
            );
            ensure!(!entry.token.is_empty(), "tokens must not be empty");
            ensure!(!entry.scopes.is_empty(), "token scopes must not be empty");
            ensure!(
                seen.insert(&entry.token),
                "duplicate token in configuration"
            );
        }
        Ok(Self {
            tokens: Arc::new(tokens),
            ingress: Arc::new(Vec::new()),
            public: false,
        })
    }
    pub fn tenants(&self) -> BTreeSet<TenantId> {
        self.tokens
            .iter()
            .map(|entry| entry.tenant.clone())
            .collect()
    }
    pub fn with_ingress_bearer(self, bearer: Option<String>) -> Self {
        self.with_tenant_ingress(TenantId::default(), bearer)
    }
    pub fn with_tenant_ingress(mut self, tenant: TenantId, bearer: Option<String>) -> Self {
        let entries = Arc::make_mut(&mut self.ingress);
        entries.retain(|entry| entry.tenant != tenant);
        if let Some(bearer) = bearer {
            entries.push(TenantIngress { tenant, bearer });
        }
        self
    }
    pub(crate) fn ingress_tenant(&self, candidate: &str) -> Option<TenantId> {
        let mut tenant = None;
        for entry in self.ingress.iter() {
            if bool::from(entry.bearer.as_bytes().ct_eq(candidate.as_bytes())) {
                // Ambiguous cluster credentials cannot select an authority.
                if tenant.is_some() {
                    return None;
                }
                tenant = Some(entry.tenant.clone());
            }
        }
        tenant
    }
    pub(crate) fn is_ingress(&self, candidate: &str) -> bool {
        self.ingress
            .iter()
            .any(|entry| bool::from(entry.bearer.as_bytes().ct_eq(candidate.as_bytes())))
    }
    pub fn authenticate(&self, candidate: &str) -> Option<Access> {
        let mut access = None;
        for entry in self.tokens.iter() {
            if bool::from(entry.token.as_bytes().ct_eq(candidate.as_bytes())) {
                access = Some(Access {
                    scopes: entry.scopes.clone(),
                    tenant: entry.tenant.clone(),
                });
            }
        }
        access
    }
}
pub fn command_scope(command: &str) -> Scope {
    if let Some(verb) = loom_proto::verbs::lookup(command) {
        return match verb.permission {
            loom_proto::verbs::Permission::Read => Scope::Read,
            loom_proto::verbs::Permission::Execute => Scope::Execute,
            loom_proto::verbs::Permission::Define => Scope::Define,
        };
    }
    match command {
        "cas.list" | "cas.inspect" | "trace.effects" | "defs" | "events" | "resolve" | "deps"
        | "build" | "stats" | "process.list" | "process.status" => Scope::Read,
        "machine.create" | "process.start" | "model.state" | "model.list" => Scope::Host,
        "backup" | "gc" | "cache_evict" => Scope::Admin,
        _ => Scope::Execute,
    }
}
pub fn request_scope(command: &str, args: &serde_json::Value) -> Scope {
    if command == "view" && args.get("actor").is_some() {
        Scope::Execute
    } else {
        command_scope(command)
    }
}
