use libloading::{Library, Symbol};
use std::time::Instant;

#[repr(C)]
struct Frame { ptr: *mut u8, len: usize }
type Entry = unsafe extern "C" fn(*const u8, usize, *mut Frame) -> i32;

fn call(lib: &Library, name: &[u8], args: &[u8]) -> Vec<u8> {
    let entry: Symbol<Entry> = unsafe { lib.get(name).unwrap() };
    let mut frame = Frame { ptr: std::ptr::null_mut(), len: 0 };
    assert_eq!(unsafe { entry(args.as_ptr(), args.len(), &mut frame) }, 0);
    let bytes = unsafe { std::slice::from_raw_parts(frame.ptr, frame.len) }.to_vec();
    let free: Symbol<unsafe extern "C" fn(*mut u8, usize)> = unsafe { lib.get(b"loom_dealloc").unwrap() };
    unsafe { free(frame.ptr, frame.len) };
    bytes
}

fn bench(label: &str, lib: &Library, name: &[u8], args: &[u8], runs: usize) {
    let mut us = Vec::new();
    let mut out = 0;
    for _ in 0..runs {
        let t = Instant::now();
        out = call(lib, name, args).len();
        us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("{label:<34} args {:>8} B  out {:>7} B  median {:>10.1} us  min {:>10.1} us", args.len(), out, us[us.len() / 2], us[0]);
}

fn main() {
    let path = std::env::args().nth(1).expect("path to libguest");
    let t = Instant::now();
    let lib = unsafe { Library::new(&path).unwrap() };
    println!("dlopen {:.0} us", t.elapsed().as_secs_f64() * 1e6);
    bench("primes(200_000)", &lib, b"loom_call_primes", &serde_ipld_dagcbor::to_vec(&(200_000u32,)).unwrap(), 50);
    bench("scene(0): 424 commands out", &lib, b"loom_call_scene", &serde_ipld_dagcbor::to_vec(&(0u32,)).unwrap(), 200);
    let floats: Vec<f32> = (0..1_000_000).map(|i| (i % 1000) as f32 * 0.5).collect();
    bench("sum 1M f32 as CBOR array", &lib, b"loom_call_sum_array", &serde_ipld_dagcbor::to_vec(&(floats.clone(),)).unwrap(), 20);
    let packed: Vec<u8> = floats.iter().flat_map(|f| f.to_le_bytes()).collect();
    bench("sum 1M f32 as CBOR byte string", &lib, b"loom_call_sum_packed", &serde_ipld_dagcbor::to_vec(&(serde_bytes::ByteBuf::from(packed),)).unwrap(), 20);
}
