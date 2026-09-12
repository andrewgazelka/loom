use super::*;
impl ActorService {
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
        self.node
            .send(
                &args.id,
                &key,
                &serde_json::to_vec(&args.msg).map_err(error)?,
            )
            .await
            .map_err(error)?;
        self.node.run_until_idle().await.map_err(error)?;
        json_value(json!({"cursor":self.node.info(&args.id).await.map_err(error)?.cursor}))
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
        let mut spec = serde_json::to_value(ChildSpec::new(
            &args.behavior_hash,
            &init,
            self.node
                .behavior(&args.behavior_hash)
                .map_err(error)?
                .child_type(),
        ))
        .map_err(error)?;
        if let Some(options) = args.spec {
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
                    "restart" | "shutdown" | "link" | "monitor" | "type"
                ) {
                    return Err(error(format!(
                        "actor {parent} seq -1: unknown spec field {key}"
                    )));
                }
                spec[key] = value.clone();
            }
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
            .promote(&args.id, &args.behavior_hash, &args.author, &args.rationale)
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
        self.node.behavior(&args.new_hash).map_err(error)?;
        for id in self.node.actor_ids().map_err(error)? {
            let info = self.node.info(&id).await.map_err(error)?;
            if info.behavior_hash == args.old_hash && info.status != loom_actor::Status::Stopped {
                self.authority(&id, Rights::PROMOTE, "actor_promote_where")
                    .await?;
            }
        }
        json_value(
            self.node
                .promote_where(
                    &args.old_hash,
                    &args.new_hash,
                    &args.author,
                    &args.rationale,
                )
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
