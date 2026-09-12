//! Native behaviors available on every node.
use crate::{Behavior, Cap, Ctx, Registry, Trap};
use async_trait::async_trait;
use std::sync::Arc;

pub struct Counter {
    pub hash: &'static str,
    pub upgraded: bool,
    pub extra_effect: bool,
    pub target: Option<Cap>,
    pub swallow_effect_errors: bool,
}

impl Counter {
    pub fn plain() -> Self {
        Self { hash: "counter-v1", upgraded: false, extra_effect: false, target: None, swallow_effect_errors: false }
    }
}

#[async_trait]
impl Behavior for Counter {
    fn description(&self) -> &str {
        "Records each message in the entries table."
    }
    fn hash(&self) -> &str {
        self.hash
    }

    fn schema(&self) -> &str {
        if self.upgraded {
            "ALTER TABLE entries ADD COLUMN revision TEXT"
        } else {
            "CREATE TABLE IF NOT EXISTS entries(seq INTEGER, body BLOB, implementation TEXT)"
        }
    }

    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        if msg == b"poison" && !self.upgraded {
            return Err(Trap::new("counter rejects poison"));
        }
        let seq = cx.seq();
        if self.upgraded {
            cx.sql(
                "INSERT INTO entries(seq, body, implementation, revision) VALUES (?1, ?2, ?3, 'added')",
                turso::params![seq, msg, self.hash],
            )
            .await?;
        } else {
            cx.sql("INSERT INTO entries(seq, body, implementation) VALUES (?1, ?2, ?3)", turso::params![seq, msg, self.hash]).await?;
        }
        if let Some(target) = &self.target {
            cx.send(target, msg).await?;
        }
        if msg == b"effect" || self.extra_effect {
            if self.swallow_effect_errors {
                let _ = cx.effect("echo", b"recorded").await;
            } else {
                cx.effect("echo", b"recorded").await?;
            }
        }
        if msg == b"request" {
            cx.request("echo", b"x").await?;
        }
        Ok(())
    }
}

pub struct Forwarder {
    pub trap: bool,
}

#[async_trait]
impl Behavior for Forwarder {
    fn description(&self) -> &str {
        "Sends forwarded to the capability carried in each message."
    }
    fn hash(&self) -> &str {
        if self.trap { "forwarder-trap" } else { "forwarder-v1" }
    }

    fn schema(&self) -> &str {
        ""
    }

    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let target: Cap = serde_json::from_slice(msg).map_err(|error| Trap::new(error.to_string()))?;
        cx.accept(target.clone()).await?;
        cx.send(&target, b"forwarded").await?;
        if self.trap {
            return Err(Trap::new("trap after send"));
        }
        Ok(())
    }
}

pub struct Echo;
#[async_trait]
impl Behavior for Echo {
    fn description(&self) -> &str {
        "Replies to call envelopes with their original payload."
    }
    fn hash(&self) -> &str {
        "echo-v1"
    }
    fn schema(&self) -> &str {
        ""
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(msg) else {
            return Ok(());
        };
        if value["type"] != "call" {
            return Ok(());
        }
        let from: Cap = serde_json::from_value(value["reply_cap"].clone()).map_err(|e| Trap::new(format!("call reply_cap: {e}")))?;
        cx.accept(from.clone()).await?;
        let reference = value["ref"].as_str().ok_or_else(|| Trap::new("call missing ref"))?;
        let payload: Vec<u8> = serde_json::from_value(value["msg"].clone()).map_err(|e| Trap::new(e.to_string()))?;
        cx.reply(&from, reference, &payload).await
    }
}

pub(crate) fn register(registry: &mut Registry) {
    for behavior in [Arc::new(Counter::plain()) as Arc<dyn Behavior>, Arc::new(Forwarder { trap: false }), Arc::new(Echo)] {
        registry.entry(behavior.hash().to_owned()).or_insert(behavior);
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct BehaviorInfo {
    pub hash: String,
    pub description: String,
}
