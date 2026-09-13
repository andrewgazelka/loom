//! HTTP transport owns cache invalidation; the outbox owns retry and FIFO.
use crate::{Ack, DeliveryOp, Node, Placement};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct IngressRequest {
    pub ops: Vec<DeliveryOp>,
}

#[derive(Serialize, Deserialize)]
pub struct IngressResponse {
    pub acks: Vec<Ack>,
    pub applied: usize,
}
impl IngressResponse {
    pub fn new(acks: Vec<Ack>) -> Self {
        let applied = acks.iter().take_while(|ack| ack.ok).count();
        Self { acks, applied }
    }
}

impl Node {
    pub fn is_ingress_bearer(&self, candidate: &str) -> bool {
        self.ingress_bearer().is_some_and(|expected| {
            let difference = expected.as_bytes().iter().zip(candidate.as_bytes()).fold(0u8, |d, (a, b)| d | (a ^ b));
            expected.len() == candidate.len() && difference == 0
        })
    }

    pub async fn forward(&self, addr: &str, ops: &[DeliveryOp]) -> Result<Vec<Ack>> {
        let result = async {
            let bearer = self.ingress_bearer().context("--cluster-key-file is required for ingress forwarding")?;
            let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(self.config.lease_ttl).build()?;
            let response = client
                .post(format!("http://{addr}/v1/ingress"))
                .bearer_auth(bearer)
                .json(&IngressRequest { ops: ops.to_vec() })
                .send()
                .await?;
            let status = response.status();
            ensure!(
                status.is_success() || status == reqwest::StatusCode::CONFLICT || status.is_server_error(),
                "ingress {addr}: HTTP {status}"
            );
            let response: IngressResponse = response.json().await?;
            ensure!(
                response.applied == response.acks.iter().take_while(|ack| ack.ok).count(),
                "ingress {addr}: applied count does not match acknowledgements"
            );
            let acks = response.acks;
            ensure!(!acks.is_empty() || ops.is_empty(), "ingress {addr}: empty acknowledgement");
            ensure!(acks.len() <= ops.len(), "ingress {addr}: too many acknowledgements");
            ensure!(acks.iter().take(acks.len().saturating_sub(1)).all(|ack| ack.ok), "ingress {addr}: non-prefix acknowledgement");
            ensure!(
                acks.len() == ops.len() || acks.last().is_some_and(|ack| !ack.ok),
                "ingress {addr}: truncated successful acknowledgement"
            );
            ensure!(
                status.is_success() || acks.last().is_some_and(|ack| !ack.ok),
                "ingress {addr}: failure status with successful acknowledgement"
            );
            if !status.is_success() || acks.iter().any(|ack| !ack.ok) {
                for op in ops {
                    self.invalidate_placement(&op.target()?)?;
                }
            }
            Ok(acks)
        }
        .await;
        if result.is_err() {
            for op in ops {
                self.invalidate_placement(&op.target()?)?;
            }
        }
        result
    }

    pub(crate) async fn route_outbox(&self, sender: &str, seq: i64, op: DeliveryOp) -> Result<bool> {
        if let DeliveryOp::Spawn { delivery } = &op
            && matches!(serde_json::from_slice::<crate::Spawn>(&delivery.msg)?, crate::Spawn::Restart { .. })
            && self.children_shutting_down(sender).await?
        {
            return Ok(false);
        }
        let target = op.target()?;
        match self.resolve(&target).await? {
            Placement::Remote { addr, .. } => {
                // Durable revisions and inbox sequence numbers are different clocks.
                // Flush all sender changes, including control writes at an old seq.
                self.ship_inner(sender).await.with_context(|| format!("actor {sender} seq {seq}: output gate"))?;
                self.check_lease(sender)?;
                let acks = self.forward(&addr, &[op]).await?;
                ensure!(acks.len() == 1, "actor {sender} seq {seq}: ingress returned wrong ack count");
                let ack = &acks[0];
                ensure!(ack.ok, "actor {sender} seq {seq}: ingress: {}", ack.error.as_deref().unwrap_or("ownership changed"));
                Ok(true)
            }
            Placement::Local | Placement::Unowned => Box::pin(self.apply_delivery(op)).await,
        }
    }
}
