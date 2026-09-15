//! Linux VMs use the ordinary durable process mailbox and capability handles.
use crate::{
    Behavior, Ctx, Trap,
    drivers::vm::HASH as DRIVER_HASH,
    process_actor::{ProcessActor, subscribe},
};
use async_trait::async_trait;
use loom_proto::VmSpec;
use serde::Deserialize;

pub const HASH: &str = "vm-v1";
pub struct VmActor;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Init {
    vm: VmSpec,
    subscriber: Option<Vec<u8>>,
}
#[async_trait]
impl Behavior for VmActor {
    fn hash(&self) -> &str {
        HASH
    }
    fn schema(&self) -> &str {
        ProcessActor.schema()
    }
    fn description(&self) -> &str {
        "Temporary Linux VM with durable stdin, output and exit mailbox."
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        if !cx.sql("SELECT id FROM process_state WHERE id=1", ()).await?.rows.is_empty() {
            return ProcessActor.handle(cx, msg).await;
        }
        let init: Init = serde_json::from_slice(msg).map_err(|error| Trap::new(format!("VM init: {error}")))?;
        init.vm.validate().map_err(Trap::new)?;
        let bytes = serde_json::to_vec(&init.vm).map_err(|error| Trap::new(error.to_string()))?;
        let cap = cx.spawn_driver(DRIVER_HASH, &bytes).await?;
        cx.sql(
            "INSERT INTO process_state(id,preset,preset_name,driver_cap,phase) VALUES (1,?,?,?,'starting')",
            turso::params![
                DRIVER_HASH,
                init.vm.image.reference,
                serde_json::to_string(&cap).map_err(|error| Trap::new(error.to_string()))?
            ],
        )
        .await?;
        if let Some(cap) = init.subscriber {
            subscribe(cx, cap).await?;
        }
        Ok(())
    }
}
