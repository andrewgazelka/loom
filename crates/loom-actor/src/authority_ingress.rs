//! Capability authority stays with the leased actor, including revocation reads.
use crate::{Cap, DeliveryOp, EffectError, Node, Rights, SqlValue};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize)]
struct AuthorityResult {
    value: Option<Value>,
    invalid: Option<String>,
}

fn encode(result: Result<Value, EffectError>) -> Result<Value> {
    let response = match result {
        Ok(value) => AuthorityResult { value: Some(value), invalid: None },
        Err(EffectError::Deterministic(error)) => AuthorityResult { value: None, invalid: Some(format!("{error:#}")) },
        Err(EffectError::Environmental(error)) => return Err(error),
    };
    serde_json::to_value(response).context("serialize ingress authority result")
}

impl Node {
    pub(crate) async fn remote_authority(&self, addr: &str, op: DeliveryOp) -> Result<Value, EffectError> {
        let result = async {
            let acks = self.forward(addr, &[op]).await?;
            ensure!(acks.len() == 1, "ingress authority: expected one acknowledgement, received {}", acks.len());
            let ack = acks.into_iter().next().context("ingress authority: missing acknowledgement")?;
            ensure!(ack.ok, "ingress authority: {}", ack.error.as_deref().unwrap_or("placement changed"));
            let response: AuthorityResult = serde_json::from_value(ack.result.context("ingress authority: missing result")?)?;
            Ok(response)
        }
        .await;
        let response: AuthorityResult = result.map_err(EffectError::Environmental)?;
        match response {
            AuthorityResult { value: Some(value), invalid: None } => Ok(value),
            AuthorityResult { value: None, invalid: Some(message) } => Err(EffectError::Deterministic(anyhow::anyhow!(message))),
            _ => Err(EffectError::Environmental(anyhow::anyhow!("ingress authority: malformed result"))),
        }
    }

    pub(crate) async fn ingress_authority(&self, cap: &Cap, right: Rights, operation: &str) -> Result<Value> {
        let result = async {
            let reader = self.capability_reader(&cap.target).await?;
            self.verify_cap_on(&reader, cap, right, operation).await?;
            Ok(Value::Bool(true))
        }
        .await;
        encode(result)
    }

    pub(crate) async fn ingress_mint(&self, target: &str, rights: Rights) -> Result<Value> {
        let cap = self.cap_for_inner(target, rights).await?;
        encode(serde_json::to_value(cap).map_err(|error| EffectError::Environmental(error.into())))
    }

    pub(crate) async fn ingress_inspect(&self, cap: &Cap, query: Option<&str>, params: Vec<SqlValue>) -> Result<Value> {
        let result = async {
            let reader = self.capability_reader(&cap.target).await?;
            self.verify_cap_on(&reader, cap, Rights::INSPECT, "inspect").await?;
            match query {
                Some(sql) => {
                    let bytes = crate::cap_inspection::query_on(&reader, cap, sql, params).await?;
                    serde_json::from_slice(&bytes).map_err(|error| EffectError::Environmental(error.into()))
                }
                None => {
                    let state = crate::cap_inspection::state_on(&reader, cap).await.map_err(EffectError::Environmental)?;
                    serde_json::to_value(state).map_err(|error| EffectError::Environmental(error.into()))
                }
            }
        }
        .await;
        encode(result)
    }
}
