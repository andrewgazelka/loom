use super::*;
impl ActorService {
    pub(super) async fn actor_view(&self, args: ViewArgs) -> anyhow::Result<Value> {
        let source = self.authority(&args.actor, Rights::INSPECT, "view").await?;
        let parent = self.node.root();
        self.authority(&parent, Rights::SPAWN, "view").await?;
        let init = serde_json::to_vec(
            &json!({"source":source,"table":args.table,"template":args.template,"order_by":args.order_by}),
        )?;
        let mut spec = ChildSpec::new("view-v1", &init, loom_actor::ChildType::Worker);
        spec.durability = loom_actor::Durability::Ephemeral;
        let id = self.node.spawn(&parent, &spec).await.map_err(error)?;
        self.node.run_until_idle().await.map_err(error)?;
        let info = self.node.info(&id).await.map_err(error)?;
        anyhow::ensure!(
            info.status == loom_actor::Status::Running,
            "actor {id} seq {}: view initialization: {}",
            info.cursor,
            info.reason
        );
        let cap = self
            .node
            .cap_for(&id, Rights::INSPECT)
            .await
            .map_err(error)?;
        // Browser JSON numbers cannot represent every u64 capability ID; keep the token opaque.
        Ok(json!({"id":id,"cap":serde_json::to_string(&cap)?}))
    }
    pub(super) async fn actor_list(&self) -> anyhow::Result<Value> {
        let mut list = Vec::new();
        for id in self.node.actor_ids().map_err(error)? {
            self.authority(&id, Rights::INSPECT, "actor_list").await?;
            let info = self.node.info(&id).await.map_err(error)?;
            list.push(json!({"id":id,"status":info.status,"behavior_hash":info.behavior_hash,"cursor":info.cursor,"inbox_len":info.inbox_len,"parent":info.parent}));
        }
        json_value(list)
    }
    pub(super) async fn actor_tree(&self, args: TreeArgs) -> anyhow::Result<Value> {
        json_value(
            self.tree(&args.root.unwrap_or_else(|| self.node.root()))
                .await
                .map_err(error)?,
        )
    }
    pub(super) async fn actor_info(&self, args: IdArgs) -> anyhow::Result<Value> {
        self.authority(&args.id, Rights::INSPECT, "actor_info")
            .await?;
        json_value(self.node.info(&args.id).await.map_err(error)?)
    }
    pub(super) async fn actor_send(&self, args: SendArgs) -> anyhow::Result<Value> {
        self.authority(&args.id, Rights::SEND, "actor_send").await?;
        let key = args
            .key
            .unwrap_or_else(|| format!("mcp:{}", uuid::Uuid::new_v4()));
        match self
            .node
            .send_with_outcome(&args.id, &key, &serde_json::to_vec(&args.msg)?)
            .await?
        {
            loom_actor::SendOutcome::Complete { id, seq, cursor } => {
                Ok(json!({"id":id,"seq":seq,"cursor":cursor}))
            }
            loom_actor::SendOutcome::Failed { id, seq, cause, .. } => {
                Err(crate::message_failure::ActorMessageFailure {
                    id,
                    seq,
                    cause,
                    outcome: crate::message_failure::MessageOutcome::Failed,
                }
                .into())
            }
            loom_actor::SendOutcome::Pending { id, seq, cause, .. } => {
                Err(crate::message_failure::ActorMessageFailure {
                    id,
                    seq,
                    cause,
                    outcome: crate::message_failure::MessageOutcome::Pending,
                }
                .into())
            }
        }
    }

    pub(super) async fn actor_spawn(&self, args: SpawnArgs) -> anyhow::Result<Value> {
        let parent = args.parent.unwrap_or_else(|| self.node.root());
        self.authority(&parent, Rights::SPAWN, "actor_spawn")
            .await?;
        let init = if args.init.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&args.init).map_err(error)?
        };
        let child_type = if args.r#def == "view-v1" {
            loom_actor::ChildType::Worker
        } else {
            self.node
                .behavior(&args.r#def)
                .await
                .map_err(error)?
                .child_type()
        };
        let mut spec =
            serde_json::to_value(ChildSpec::new(&args.r#def, &init, child_type)).map_err(error)?;
        // ChildSpec::deserialize selects restart from the final durability; only a caller's policy is explicit.
        spec.as_object_mut()
            .expect("serialized child spec")
            .remove("restart");
        if let Some(options) = &args.spec {
            let options = options
                .as_object()
                .ok_or_else(|| error(format!("actor {parent} seq -1: spec must be an object")))?;
            if options.contains_key("type") && !options.contains_key("shutdown") {
                spec.as_object_mut()
                    .expect("serialized child spec")
                    .remove("shutdown");
            }
            for (key, value) in options {
                if matches!(key.as_str(), "behavior_hash" | "init") {
                    return Err(error(format!(
                        "actor {parent} seq -1: spec cannot override behavior_hash or init"
                    )));
                }
                if !matches!(
                    key.as_str(),
                    "restart" | "shutdown" | "link" | "monitor" | "type" | "durability"
                ) {
                    return Err(error(format!(
                        "actor {parent} seq -1: unknown spec field {key}"
                    )));
                }
                spec[key] = value.clone();
            }
        }
        if let Some(durability) = args.durability {
            if args
                .spec
                .as_ref()
                .is_some_and(|options| options.get("durability").is_some())
            {
                return Err(error(format!(
                    "actor {parent} seq -1: durability supplied twice"
                )));
            }
            spec["durability"] = serde_json::to_value(durability)?;
        }
        let spec: ChildSpec = serde_json::from_value(spec).map_err(error)?;
        let id = self.node.spawn(&parent, &spec).await.map_err(error)?;
        json_value(json!({"id":id}))
    }
    pub(super) async fn actor_stop(&self, args: StopArgs) -> anyhow::Result<Value> {
        self.authority(&args.id, Rights::STOP, "actor_stop").await?;
        self.node
            .stop(&args.id, &args.reason)
            .await
            .map_err(error)?;
        json_value(json!({"id":args.id}))
    }
    pub(super) async fn actor_restart(&self, args: RestartArgs) -> anyhow::Result<Value> {
        self.authority(&args.id, Rights::SPAWN, "actor_restart")
            .await?;
        let verb = serde_json::from_value(json!(args.verb))
            .map_err(|e| error(format!("actor {} seq -1: {e}", args.id)))?;
        self.node.restart(&args.id, verb).await.map_err(error)?;
        json_value(json!({"id":args.id}))
    }
    pub(super) async fn actor_promote(&self, args: PromoteArgs) -> anyhow::Result<Value> {
        self.authority(&args.id, Rights::PROMOTE, "actor_promote")
            .await?;
        self.node
            .promote(&args.id, &args.hash, &args.author, &args.rationale)
            .await
            .map_err(error)?;
        let rows = self
            .rows(
                "actor_promote",
                &args.id,
                "SELECT * FROM code_changes ORDER BY seq DESC LIMIT 1",
                vec![],
            )
            .await?;
        json_value(&rows[0])
    }
    pub(super) async fn actor_promote_where(
        &self,
        args: PromoteWhereArgs,
    ) -> anyhow::Result<Value> {
        self.node.behavior(&args.new).await.map_err(error)?;
        for id in self.node.actor_ids().map_err(error)? {
            let info = self.node.info(&id).await.map_err(error)?;
            if info.behavior_hash == args.old && info.status != loom_actor::Status::Stopped {
                self.authority(&id, Rights::PROMOTE, "actor_promote_where")
                    .await?;
            }
        }
        json_value(
            self.node
                .promote_where(&args.old, &args.new, &args.author, &args.rationale)
                .await
                .map_err(error)?,
        )
    }
    pub(super) async fn actor_lineage(&self, args: IdArgs) -> anyhow::Result<Value> {
        json_value(
            self.rows(
                "actor_lineage",
                &args.id,
                "SELECT * FROM code_changes ORDER BY seq",
                vec![],
            )
            .await?,
        )
    }
}
