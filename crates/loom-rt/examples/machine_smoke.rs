mod support;
use anyhow::{Context, Result, ensure};
use loom_proto::Lang;
use loom_rt::Runtime;
use loom_store::Store;
use serde_json::json;
#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let rust = args.next().context("Rust component required")?;
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("small"), "abc")?;
    std::fs::write(root.path().join("largest"), "123456789")?;
    std::fs::create_dir(root.path().join("directory"))?;
    let store = Store::memory()?;
    let runtime = Runtime::new(store.clone())?;
    let machine = runtime.create_machine(root.path())?;
    for fixture in [Fixture {
        path: rust,
        lang: Lang::Rust,
    }] {
        let hash = support::register(&store, fixture.lang, fixture.path, "machine fixture")?;
        let result = runtime.call_def(&hash, json!([machine.id, "/"])).await?;
        ensure!(
            result["name"] == "largest" && result["size"] == 9,
            "largest file mismatch {result}"
        );
        println!("{} largest-file machine actor pass", fixture.lang.as_str());
    }
    Ok(())
}
struct Fixture {
    path: String,
    lang: Lang,
}
