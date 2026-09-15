//! Container resources share the process actor mailbox, persistence and handles.
use crate::{
    Behavior, Ctx, Trap,
    drivers::container::{ContainerSpec, HASH as DRIVER_HASH},
    process_actor::{ProcessActor, subscribe},
};
use async_trait::async_trait;
use serde::Deserialize;
pub const HASH: &str = "container-v1";
pub struct ContainerActor;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Init {
    container: ContainerSpec,
    subscriber: Option<Vec<u8>>,
}
#[async_trait]
impl Behavior for ContainerActor {
    fn hash(&self) -> &str {
        HASH
    }
    fn schema(&self) -> &str {
        ProcessActor.schema()
    }
    fn description(&self) -> &str {
        "Temporary Docker container with durable stdin, output and exit mailbox."
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        if !cx.sql("SELECT id FROM process_state WHERE id=1", ()).await?.rows.is_empty() {
            return ProcessActor.handle(cx, msg).await;
        }
        let init: Init = serde_json::from_slice(msg).map_err(|e| Trap::new(format!("container init: {e}")))?;
        init.container.validate().map_err(|e| Trap::new(e.to_string()))?;
        let bytes = serde_json::to_vec(&init.container).map_err(|e| Trap::new(e.to_string()))?;
        let cap = cx.spawn_driver(DRIVER_HASH, &bytes).await?;
        cx.sql(
            "INSERT INTO process_state(id,preset,preset_name,driver_cap,phase) VALUES (1,?,?,?,'starting')",
            turso::params![DRIVER_HASH, init.container.image, serde_json::to_string(&cap).map_err(|e| Trap::new(e.to_string()))?],
        )
        .await?;
        if let Some(cap) = init.subscriber {
            subscribe(cx, cap).await?;
        }
        Ok(())
    }
}
