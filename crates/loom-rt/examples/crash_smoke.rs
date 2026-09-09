mod support;
use anyhow::{Context, Result, ensure};
use loom_proto::Lang;
use loom_rt::Runtime;
use loom_store::Store;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().context("mode required")?;
    if mode == "child" {
        let database = args.next().context("database required")?;
        let runtime = Runtime::new(Store::open(database)?)?;
        ensure!(
            runtime.recover_pending().await? == 1,
            "expected one pending message"
        );
        return Ok(());
    }
    let component = args.next().context("component required")?;
    let temp = tempfile::tempdir()?;
    let database = temp.path().join("loom.sqlite");
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    let ready = temp.path().join("ready");
    let gate = temp.path().join("gate");
    let store = Store::open(&database)?;
    let hash = support::register(&store, Lang::Rust, component, "crash fixture")?;
    let runtime = Runtime::new(store.clone())?;
    let actor = runtime.spawn(&hash, json!(0)).await?;
    store.enqueue(
        &actor.id,
        &json!({"first_path":first,"second_path":second,"ready_path":ready,"gate_path":gate}),
    )?;
    let executable = std::env::current_exe()?;
    let mut child = tokio::process::Command::new(&executable)
        .arg("child")
        .arg(&database)
        .kill_on_drop(true)
        .spawn()?;
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if ready.exists() {
                break;
            }
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("child exited before ready: {status}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    child.kill().await?;
    child.wait().await?;
    ensure!(
        std::fs::read_to_string(&first)? == "x",
        "first effect not performed once"
    );
    ensure!(!second.exists(), "second completed before kill");
    std::fs::write(&gate, "resume")?;
    let status = tokio::process::Command::new(executable)
        .arg("child")
        .arg(&database)
        .kill_on_drop(true)
        .status()
        .await?;
    ensure!(status.success(), "recovery child failed");
    ensure!(
        std::fs::read_to_string(first)? == "x",
        "cached first effect repeated"
    );
    ensure!(
        std::fs::read_to_string(second)? == "x",
        "second effect not exactly once"
    );
    ensure!(
        store.pending_messages()?.is_empty(),
        "message remains pending"
    );
    ensure!(
        runtime.state(&actor.id).await? == json!(1),
        "state not committed"
    );
    println!(
        "M3: SIGKILL/restart replay hit first effect; second performed once; pending message committed"
    );
    Ok(())
}
