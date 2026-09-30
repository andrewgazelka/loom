//! Costs of the kernel path (section 7 of docs/design/host-kernels.md), measured.
//! Run: cargo +nightly-2026-09-05 run --release --bin bench
use glam::DVec3;
use loom_rt::{Handle, HostKernel, KernelContext, Runtime};
use loom_store::Store;
use rgb_host_kernels::{RgbHost, TriBvh};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Ops with known cost, to isolate the boundary: `nop` returns nothing, `big` a megabyte.
struct Bench;
impl HostKernel for Bench {
    fn family(&self) -> &str {
        "bench"
    }
    fn version(&self) -> u32 {
        1
    }
    fn ops(&self) -> &[&'static str] {
        &["nop", "big"]
    }
    fn call(&self, _: &KernelContext<'_>, op: &str, _: &[&[u8]]) -> Result<Vec<u8>, String> {
        Ok(if op == "big" { vec![7u8; 1 << 20] } else { Vec::new() })
    }
}

/// A guest whose `main` calls `op` `loops` times with `buffers` 24-byte buffers, then answers CBOR 7.
fn guest(loops: u32, op: &str, buffers: u32) -> Vec<u8> {
    let iov: String = (0..buffers).map(|_| "\\00\\04\\00\\00\\18\\00\\00\\00").collect(); // ptr 1024, len 24
    let mut artifact = wat::parse_str(format!(
        r#"(module
        (import "env" "memory" (memory 40 40 shared))
        (import "loom" "kernel" (func $kernel (param i32 i32 i32 i32) (result i64)))
        (global (export "__stack_pointer") (mut i32) (i32.const 65536))
        (global (export "__loom_stack_low") (mut i32) (i32.const 32768))
        (global (export "__loom_stack_high") (mut i32) (i32.const 65536))
        (global $heap (mut i32) (i32.const 8192))
        (func $alloc (export "loom_alloc") (param $size i32) (param i32) (result i32)
            (local $pointer i32)
            ;; a bump allocator that recycles: reset when a big reply would overflow one page
            global.get $heap local.tee $pointer local.get $size i32.add global.set $heap
            local.get $pointer)
        (func (export "loom_dealloc") (param i32 i32 i32))
        (data (i32.const 512) "{op}")
        (data (i32.const 1024) "\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00")
        (data (i32.const 2048) "{iov}")
        (func (export "loom_call_main") (param i32 i32) (result i64)
            (local $i i32)
            (loop $again
                i32.const 512 i32.const {oplen} i32.const 2048 i32.const {buffers} call $kernel drop
                ;; reclaim the reply allocation: reset the heap to its start each call
                i32.const 8192 global.set $heap
                local.get $i i32.const 1 i32.add local.tee $i
                i32.const {loops} i32.lt_u br_if $again)
            i32.const 8192 global.set $heap
            i32.const 2 i32.const 1 call $alloc drop
            i32.const 8192 i32.const 0 i32.store8
            i32.const 8193 i32.const 7 i32.store8
            i64.const 2 i64.const 32 i64.shl i64.const 8192 i64.or)
    )"#,
        oplen = op.len(),
    ))
    .unwrap();
    loom_proto::core_protocol::stamp(&mut artifact);
    artifact
}

fn register(store: &Store, artifact: &[u8], salt: &str) -> String {
    let component = store.put("component", artifact).unwrap();
    let source = format!("bench fixture {salt} {component}");
    let deps = std::collections::BTreeMap::new();
    let hash = blake3::hash(&loom_proto::definition_identity(loom_proto::Lang::Rust, &source, &deps, None).unwrap())
        .to_hex()
        .to_string();
    let effects = loom_proto::EffectSet { labels: vec!["kernel".into()], unknown: false };
    store
        .define(
            &loom_proto::Def {
                hash: hash.clone(),
                lang: loom_proto::Lang::Rust,
                component_hash: Some(component),
                sig: loom_proto::TypeSig {
                    exports: vec![loom_proto::ExportSig {
                        name: "main".into(),
                        params: vec![],
                        returns: loom_proto::ValueShape::Value,
                        effects: effects.clone(),
                    }],
                    effects,
                },
                allowed_effects: None,
                observed_effects: Vec::new(),
            },
            None,
            &source,
            &deps,
        )
        .unwrap();
    hash
}

fn best(mut f: impl FnMut() -> Duration, runs: usize) -> Duration {
    (0..runs).map(|_| f()).min().unwrap()
}

fn main() {
    let store = Store::memory().unwrap();
    let nop = register(&store, &guest(200_000, "bench.nop", 0), "nop0");
    let nop1 = register(&store, &guest(200_000, "bench.nop", 1), "nop1");
    let big = register(&store, &guest(2_000, "bench.big", 0), "big");
    let empty = register(&store, &guest(1, "bench.nop", 0), "empty");
    let runtime = Runtime::new(store).unwrap();
    runtime.register_kernel(Arc::new(Bench)).unwrap();
    runtime.register_kernel(Arc::new(RgbHost::default())).unwrap();
    let tokio = tokio::runtime::Runtime::new().unwrap();
    let call = |hash: &str| {
        let started = Instant::now();
        tokio.block_on(runtime.call_def(hash, serde_json::json!([]))).unwrap();
        started.elapsed()
    };
    // warm: compile the modules
    for hash in [&nop, &nop1, &big, &empty] {
        call(hash);
    }
    let base = best(|| call(&empty), 9);
    println!("guest call that makes no kernel call (instantiate + return): {base:?}");
    let t = best(|| call(&nop), 5);
    println!("wasm -> host -> wasm, empty op, no buffers:   {:.0} ns per call", (t - base).as_nanos() as f64 / 200_000.0);
    let t = best(|| call(&nop1), 5);
    println!("same with one 24-byte buffer:                 {:.0} ns per call", (t - base).as_nanos() as f64 / 200_000.0);
    let t = best(|| call(&big), 5);
    println!("reply of 1 MB written into the guest:         {:.1} us per call ({:.1} GB/s)", (t - base).as_nanos() as f64 / 2_000.0 / 1e3, 2_000.0 * 1048576.0 / (t - base).as_secs_f64() / 1e9);

    // The real thing: a 250k-triangle mesh.
    let mut state = 12345u64;
    let mut rnd = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    let (segments, rings) = (700usize, 179usize);
    let vertex = |i: usize, j: usize| {
        let theta = std::f64::consts::PI * j as f64 / rings as f64;
        let phi = 2.0 * std::f64::consts::PI * (i % segments) as f64 / segments as f64;
        let r = 1.0 + 0.05 * (7.0 * theta).sin() * (5.0 * phi).cos();
        DVec3::new(r * theta.sin() * phi.cos(), r * theta.sin() * phi.sin(), r * theta.cos())
    };
    let mut triangles: Vec<[DVec3; 3]> = Vec::new();
    for j in 0..rings {
        for i in 0..segments {
            let (a, b, c, d) = (vertex(i, j), vertex(i + 1, j), vertex(i + 1, j + 1), vertex(i, j + 1));
            if j > 0 { triangles.push([a, b, d]); }
            if j + 1 < rings { triangles.push([b, c, d]); }
        }
    }
    let soup: Vec<u8> = triangles.iter().flat_map(|t| t.iter().flat_map(|p| [p.x, p.y, p.z])).flat_map(f64::to_le_bytes).collect();
    println!("\nmesh: {} triangles, {:.1} MB", triangles.len(), soup.len() as f64 / 1e6);
    let started = Instant::now();
    let handle: Handle = runtime.call_kernel("loom.put", &[&soup]).unwrap().try_into().unwrap();
    println!("loom.put (hash + CAS write):                  {:?}", started.elapsed());
    let started = Instant::now();
    let native = TriBvh::new(triangles.clone());
    let native_build = started.elapsed();
    println!("TriBvh::new native:                            {native_build:?}");
    let probe = 0.5f64.to_le_bytes().iter().chain([0u8; 16].iter()).copied().collect::<Vec<u8>>();
    let started = Instant::now();
    runtime.call_kernel("rgb-host.contains", &[&handle, &probe]).unwrap();
    println!("first query (fetch bytes + parse + build):    {:?}", started.elapsed());
    let started = Instant::now();
    runtime.call_kernel("rgb-host.contains", &[&handle, &probe]).unwrap();
    println!("second query, resident BVH, one point:        {:?}", started.elapsed());

    println!("\nrays: batch  native(1 thread)   via kernel (parallel)   kernel/native");
    for n in [1usize, 10, 100, 1_000, 10_000, 100_000] {
        let rays: Vec<(DVec3, DVec3)> = (0..n)
            .map(|_| {
                let d = DVec3::new(rnd() - 0.5, rnd() - 0.5, rnd() - 0.5).normalize();
                let target = DVec3::new(rnd() - 0.5, rnd() - 0.5, rnd() - 0.5) * 0.8;
                let origin = d * 3.0;
                (origin, (target - origin).normalize())
            })
            .collect();
        let bytes: Vec<u8> = rays.iter().flat_map(|(o, d)| [o.x, o.y, o.z, d.x, d.y, d.z, 10.0]).flat_map(f64::to_le_bytes).collect();
        let runs = if n >= 10_000 { 5 } else { 30 };
        let t_native = best(|| {
            let started = Instant::now();
            let mut hits = 0u32;
            for (o, d) in &rays {
                hits += u32::from(native.ray(*o, *d, 10.0).is_some());
            }
            std::hint::black_box(hits);
            started.elapsed()
        }, runs);
        let t_kernel = best(|| {
            let started = Instant::now();
            std::hint::black_box(runtime.call_kernel("rgb-host.ray", &[&handle, &bytes]).unwrap());
            started.elapsed()
        }, runs);
        println!("      {n:>7}  {t_native:>16.2?}  {t_kernel:>22.2?}   {:.2}x", t_kernel.as_secs_f64() / t_native.as_secs_f64());
    }
}
