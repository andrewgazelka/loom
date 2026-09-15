//! Engine-independent calls with caller-owned effects and transaction lifetime.
#![forbid(unsafe_code)]

use serde_json::Value;
use std::{future::Future, pin::Pin};

/// A deterministic guest failure, distinct from retryable host failures.
#[derive(Debug)]
pub struct GuestFailure {
    pub message: String,
}

impl GuestFailure {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GuestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GuestFailure {}

/// Root effects run serially on the caller's borrowed context. An error aborts
/// the whole call; guests cannot catch it and commit a partially failed turn.
pub trait CallEffects: Send {
    fn perform(
        &mut self,
        descriptor: Value,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + '_>>;
}

/// A prepared program. Engines own code and scheduling; callers own authority.
/// Arguments are positional JSON values. Dropping the future cancels execution
/// and releases access to the borrowed handler.
pub trait Sandbox: Send + Sync {
    fn call<'a>(
        &'a self,
        args: Value,
        effects: &'a mut dyn CallEffects,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + 'a>>;
}
