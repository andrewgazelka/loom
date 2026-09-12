use super::*;
impl ActorService {
    pub(super) async fn actor_dead_letters(&self, args: IdArgs) -> anyhow::Result<Value> {
        json_value(
            self.rows(
                "actor_dead_letters",
                &args.id,
                "SELECT * FROM dead_letters ORDER BY seq",
                vec![],
            )
            .await?,
        )
    }
    pub(super) async fn actor_fork(&self, args: ForkArgs) -> anyhow::Result<Value> {
        self.authority(&args.id, Rights::INSPECT, "actor_fork")
            .await?;
        json_value(json!({"id":self.node.fork(&args.id,args.seq).await.map_err(error)?}))
    }
    pub(super) async fn actor_validate(&self, args: ValidateArgs) -> anyhow::Result<Value> {
        self.authority(&args.id, Rights::INSPECT, "actor_validate")
            .await?;
        json_value(
            self.node
                .validate_assertions(
                    &args.id,
                    &args.candidate,
                    i64::from(args.k),
                    &args.assertions.unwrap_or_default(),
                )
                .await
                .map_err(error)?,
        )
    }
    pub(super) async fn actor_sql(&self, args: SqlArgs) -> anyhow::Result<Value> {
        json_value(
            self.rows(
                "actor_sql",
                &args.id,
                &args.query,
                args.params.unwrap_or_default(),
            )
            .await?,
        )
    }
    pub(super) async fn actor_whereis(&self, args: NameArgs) -> anyhow::Result<Value> {
        json_value(self.node.whereis(&args.name).await.map_err(error)?)
    }
    pub(super) async fn actor_register(&self, args: RegisterArgs) -> anyhow::Result<Value> {
        self.authority(&args.id, Rights::INSPECT, "actor_register")
            .await?;
        self.node
            .register(&args.name, &args.id)
            .await
            .map_err(error)?;
        json_value(json!({"id":args.id,"name":args.name}))
    }
    pub(super) async fn actor_members(&self, args: GroupArgs) -> anyhow::Result<Value> {
        json_value(self.node.members(&args.group).await.map_err(error)?)
    }
    pub(super) async fn actor_behaviors(&self) -> anyhow::Result<Value> {
        json_value(self.node.behaviors().await?)
    }
    pub(super) async fn actor_run(&self) -> anyhow::Result<Value> {
        json_value(json!({"processed":self.node.run_until_idle().await.map_err(error)?}))
    }
}
