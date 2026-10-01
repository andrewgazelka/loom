//! The engine's real-size skin weld through Loom used as a library (no HTTP, no JSON envelope, no copies of the
//! result): open a store file, add the definition, put the inputs, call it, map the result, compare it with the
//! engine's own output. `embedded_weld <state dir> <guest.rs> <Cargo.toml> <Cargo.lock> <real/ dir>`.
use loom_api::Service;
use loom_proto::{CommandRequest, Lang, StoreRef};
use loom_store::Store;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Instant};

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1e3
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (state, guest, manifest, lock, real) = (&args[1], &args[2], &args[3], &args[4], PathBuf::from(&args[5]));
    std::fs::create_dir_all(state)?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let opened = Instant::now();
    let service = Service::new(Store::open(PathBuf::from(state).join("loom.sqlite"))?, root, vec![Lang::Rust])?;
    println!("open store + runtime: {:.1} ms", ms(opened));

    let added = Instant::now();
    let reply = service
        .command(CommandRequest {
            session: None,
            command: "add".into(),
            args: json!({"name":"skin-weld","source":std::fs::read_to_string(guest)?,"lang":"rust",
                "manifest":std::fs::read_to_string(manifest)?,"lock":std::fs::read_to_string(lock)?}),
        })
        .await;
    anyhow::ensure!(reply.ok, "{:?}", reply.result);
    let hash = reply.result["hash"].as_str().unwrap().to_owned();
    println!("add skin-weld: {:.1} ms (hash {hash})", ms(added));

    let (head, body) = (std::fs::read(real.join("head.skin"))?, std::fs::read(real.join("body.skin"))?);
    let params: Value = serde_json::from_str(&std::fs::read_to_string(real.join("params.json"))?)?;
    let expected = std::fs::read(real.join("expected.weld"))?;
    let rt = &service.runtime;
    for round in 0..5 {
        let t = Instant::now();
        let (h, b) = (rt.put_blob(&head)?, rt.put_blob(&body)?);
        let upload = ms(t);
        let as_ref = |handle: &[u8; 32], len: usize| json!([handle.iter().map(|x| format!("{x:02x}")).collect::<String>(), len]);
        let call = rt
            .call_entry_cached(&hash, "", json!([as_ref(&h, head.len()), as_ref(&b, body.len()), params]))
            .await?;
        let out: StoreRef = serde_json::from_value(call.value["Ok"].clone())?;
        let mapped_at = Instant::now();
        let mapped = rt.map_ref(&out)?.expect("result object");
        let map = ms(mapped_at);
        println!(
            "round {round}: put inputs {upload:.1} ms, call {:.1} ms (cache_hit {}), map result {map:.2} ms, {} bytes, identical: {}, total {:.1} ms",
            call.run_ms,
            call.cache_hit,
            mapped.as_slice().len(),
            mapped.as_slice() == expected.as_slice(),
            ms(t)
        );
    }
    // Forced misses with the module already compiled: the call itself, no cache, no compile.
    let (h, b) = (rt.put_blob(&head)?, rt.put_blob(&body)?);
    let as_ref = |handle: &[u8; 32], len: usize| json!([handle.iter().map(|x| format!("{x:02x}")).collect::<String>(), len]);
    let mut times = Vec::new();
    for i in 1..=7 {
        let mut p = params.clone();
        p[0] = json!(p[0].as_f64().unwrap() * (1.0 + i as f64 * 1e-6));
        let t = Instant::now();
        let call = rt.call_entry_cached(&hash, "", json!([as_ref(&h, head.len()), as_ref(&b, body.len()), p])).await?;
        assert!(!call.cache_hit);
        times.push(ms(t));
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("warm miss call (module cached, 7 runs): min {:.1} median {:.1} max {:.1} ms", times[0], times[3], times[6]);
    Ok(())
}
