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
}
#[derive(Clone, Default)]
pub struct Access {
    scopes: BTreeSet<Scope>,
}
impl Access {
    pub fn owner() -> Self {
        Self {
            scopes: [Scope::Read, Scope::Execute, Scope::Define, Scope::Admin]
                .into_iter()
                .collect(),
        }
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
    pub scopes: BTreeSet<Scope>,
}
#[derive(Clone)]
pub struct Authorizer {
    tokens: Arc<Vec<TokenConfig>>,
}
impl Authorizer {
    pub fn single(token: String) -> Result<Self> {
        Self::new(vec![TokenConfig {
            token,
            scopes: Access::owner().scopes,
        }])
    }
    pub fn new(tokens: Vec<TokenConfig>) -> Result<Self> {
        ensure!(!tokens.is_empty(), "at least one token is required");
        let mut seen = BTreeSet::new();
        for entry in &tokens {
            ensure!(!entry.token.is_empty(), "tokens must not be empty");
            ensure!(!entry.scopes.is_empty(), "token scopes must not be empty");
            ensure!(
                seen.insert(&entry.token),
                "duplicate token in configuration"
            );
        }
        Ok(Self {
            tokens: Arc::new(tokens),
        })
    }
    pub fn authenticate(&self, candidate: &str) -> Option<Access> {
        let mut access = None;
        for entry in self.tokens.iter() {
            if bool::from(entry.token.as_bytes().ct_eq(candidate.as_bytes())) {
                access = Some(Access {
                    scopes: entry.scopes.clone(),
                });
            }
        }
        access
    }
}
pub fn command_scope(command: &str) -> Scope {
    match command {
        "crate.add" | "upgrade" => Scope::Define,
        "cas.list" | "cas.inspect" | "trace.effects" | "defs" | "events" | "resolve" | "deps"
        | "build" | "stats" | "process.list" | "process.status" | "model.state" | "model.list" => {
            Scope::Read
        }
        "backup" | "gc" | "cache_evict" | "machine.create" => Scope::Admin,
        _ => Scope::Execute,
    }
}
