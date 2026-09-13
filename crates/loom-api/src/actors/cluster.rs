use super::*;
use loom_actor::{Ack, DeliveryOp, Placement};

impl ActorService {
    pub(crate) async fn forward_command(&self, verb: &str, args: &Value) -> anyhow::Result<Option<Value>> {
        if matches!(verb, "move" | "whereis" | "register") {
            return Ok(None);
        }
        let target = args["id"].as_str().or_else(|| args["root"].as_str());
        let Some(target) = target else {
            return Ok(None);
        };
        let Placement::Remote { addr, .. } = self.node.resolve(target).await.map_err(error)? else {
            return Ok(None);
        };
        let op = DeliveryOp::Command {
            target: target.into(),
            verb: verb.into(),
            args: args.clone(),
        };
        let acks = self.node.forward(&addr, &[op]).await.map_err(error)?;
        let ack = acks
            .into_iter()
            .next()
            .ok_or_else(|| error(format!("actor {target} seq -1: missing command ack")))?;
        anyhow::ensure!(
            ack.ok,
            "actor {target} seq -1: {}",
            ack.error.as_deref().unwrap_or("owner refused command")
        );
        Ok(Some(ack.result.ok_or_else(|| {
            error(format!("actor {target} seq -1: missing command result"))
        })?))
    }

    pub(crate) async fn ingress_command(&self, target: String, verb: String, args: Value) -> Ack {
        let result = async {
            match self.node.resolve(&target).await? {
                Placement::Remote { node_id, addr } => {
                    return Ok(Ack {
                        ok: false,
                        conflict: true,
                        owner: Some(node_id),
                        addr: Some(addr),
                        error: None,
                        result: None,
                    });
                }
                Placement::Local | Placement::Unowned => {}
            }
            anyhow::ensure!(
                args["id"].as_str().or_else(|| args["root"].as_str()) == Some(target.as_str()),
                "actor {target} seq -1: ingress command target mismatch"
            );
            anyhow::ensure!(
                matches!(
                    verb.as_str(),
                    "info"
                        | "sql"
                        | "lineage"
                        | "dead_letters"
                        | "tree"
                        | "send"
                        | "stop"
                        | "restart"
                        | "promote"
                        | "fork"
                        | "validate"
                ),
                "actor {target} seq -1: unsupported ingress command {verb}"
            );
            // Boxing breaks the command -> forwarding -> ingress async type cycle.
            let value = Box::pin(self.command(&Access::owner(), &verb, args)).await?;
            if matches!(
                verb.as_str(),
                "send" | "stop" | "restart" | "promote" | "fork" | "validate"
            ) {
                // This ack leaves the request only after the mutation is published.
                self.node.ship(&target).await?;
            }
            Ok::<_, anyhow::Error>(Ack {
                ok: true,
                conflict: false,
                owner: None,
                addr: None,
                error: None,
                result: Some(value),
            })
        }
        .await;
        result.unwrap_or_else(|failure| Ack {
            ok: false,
            conflict: false,
            owner: None,
            addr: None,
            error: Some(format!("actor {target} seq -1: {failure:#}")),
            result: None,
        })
    }

    pub(crate) async fn cluster_actor_list(&self, cluster: bool) -> anyhow::Result<Value> {
        if !cluster {
            let mut list = self.actor_list().await?;
            if let Some(rows) = list.as_array_mut() {
                for row in rows {
                    row["owner"] = json!(self.node.identity().map(|identity| identity.node_id));
                }
            }
            return Ok(list);
        }
        let mut list = Vec::new();
        for id in self.node.cluster_actor_ids().await.map_err(error)? {
            let placement = self.node.resolve(&id).await.map_err(error)?;
            let owner = match &placement {
                Placement::Local => self.node.identity().map(|identity| identity.node_id),
                Placement::Remote { node_id, .. } => Some(node_id.clone()),
                Placement::Unowned => None,
            };
            list.push(json!({"id":id,"owner":owner,"placement":placement}));
        }
        Ok(json!(list))
    }
}
