//! One single-blob cell through Loom as a library: `embedded_cell <state dir> <guest.rs> <Cargo.toml> <Cargo.lock> <in.bin> <expected.bin>`.
//! The cell takes one `StoreRef` (the input container) and returns one (the result container); the result is compared byte for byte.
use loom_api::Service;
use loom_proto::{CommandRequest, Lang, StoreRef};
use loom_store::Store;
use serde_json::json;
use std::{path::PathBuf, time::Instant};

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1e3
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    std::fs::create_dir_all(&a[1])?;
    let service = Service::new(
        Store::open(PathBuf::from(&a[1]).join("loom.sqlite"))?,
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::Rust],
    )?;
    let t = Instant::now();
    let reply = service
        .command(CommandRequest {
            session: None,
            command: "add".into(),
            args: json!({"name":"cell","source":std::fs::read_to_string(&a[2])?,"lang":"rust",
                "manifest":std::fs::read_to_string(&a[3])?,"lock":std::fs::read_to_string(&a[4])?}),
        })
        .await;
    anyhow::ensure!(reply.ok, "{:?}", reply.result);
    let hash = reply.result["hash"].as_str().unwrap().to_owned();
    println!("add: {:.0} ms", ms(t));
    let (input, expected) = (std::fs::read(&a[5])?, std::fs::read(&a[6])?);
    let rt = &service.runtime;
    let hex = |h: &[u8; 32]| h.iter().map(|x| format!("{x:02x}")).collect::<String>();
    for round in 0..4 {
        let t = Instant::now();
        let h = rt.put_blob(&input)?;
        let put = ms(t);
        let call = rt.call_entry_cached(&hash, "", json!([[hex(&h), input.len()]])).await?;
        let out: StoreRef = serde_json::from_value(call.value["Ok"].clone())?;
        let m = Instant::now();
        let mapped = rt.map_ref(&out)?.expect("result");
        println!(
            "round {round}: put {put:.1} ms, call {:.1} ms (hit {}), map {:.2} ms, {} bytes, identical {}, total {:.1} ms",
            call.run_ms, call.cache_hit, ms(m), mapped.len(), &mapped[..] == &expected[..], ms(t)
        );
    }
    // Uncached warm calls: the same picture with one byte of the container changed is a new key, so flip the sRGB flag's
    // neighbour-free way: re-put a copy whose padding differs is not possible, so vary the first pixel instead.
    let mut times = Vec::new();
    for i in 0..7u8 {
        let mut copy = input.clone();
        let at = copy.len() - 1 - i as usize * 4;
        copy[at] ^= 1;
        let h = rt.put_blob(&copy)?;
        let t = Instant::now();
        let call = rt.call_entry_cached(&hash, "", json!([[hex(&h), copy.len()]])).await?;
        assert!(!call.cache_hit);
        times.push(ms(t));
    }
    times.sort_by(|x, y| x.partial_cmp(y).unwrap());
    println!("uncached warm (7): min {:.1} median {:.1} max {:.1} ms", times[0], times[3], times[6]);
    Ok(())
}
