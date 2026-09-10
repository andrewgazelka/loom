use anyhow::{Context, Result};
use loom_rt::Runtime;
use loom_store::Store;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let first = args.next().context("usage: handler_timing [--cancel-borrow] MODULE.wasm EXPECTED_BLAKE3")?;
    let cancellation = first == "--cancel-borrow";
    let path = if cancellation { args.next().context("cancellation module required")? } else { first };
    let expected = args.next().context("expected compiler artifact BLAKE3 is required")?;
    anyhow::ensure!(args.next().is_none(), "unexpected benchmark argument");
    let bytes = std::fs::read(&path)?;
    anyhow::ensure!(blake3::hash(&bytes).to_hex().as_str() == expected, "benchmark module does not match the compiler artifact");
    let runtime = Runtime::new(Store::memory()?)?;
    let mut result = if cancellation {
        // SAFETY: --cancel-borrow is a trusted diagnostic entrypoint. The
        // consolidated goal supplies compiler output of the repository-owned
        // effects-fixtures/cancellation.rs, whose reported live AtomicU32 uses
        // only 32-bit atomics. This mode must not run arbitrary guest modules;
        // EXPECTED_BLAKE3 checks the selected artifact, not that safety contract.
        unsafe { runtime.verify_borrowed_handler_cancellation(&bytes).await? }
    } else {
        runtime.benchmark_handler_module(&bytes).await?
    };
    let executable = std::fs::read(std::env::current_exe()?)?;
    result["engine_executable_hash"] = blake3::hash(&executable).to_hex().to_string().into();
    println!("{result}");
    anyhow::ensure!(result["pass"] == true, "native handler control failed");
    Ok(())
}
