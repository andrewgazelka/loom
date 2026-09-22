/// Isolated-call payload benchmark fixture for
/// `loom_rt::isolated::tests::isolated_call_payload_benchmark`
/// (`LOOM_ISOLATED_BENCH_MODULE`). `main(rounds, size)` performs `rounds`
/// isolated calls of `echo` on this same definition, each carrying `size`
/// payload bytes as one CBOR byte string, and returns the summed echoed
/// lengths. The host records each call's duration; the guest has no clock.
const ECHO: loom::isolated::Def<fn(loom::Bytes) -> u64> =
    loom::isolated::Def::new("$self").entry("echo");

pub fn echo(payload: loom::Bytes) -> u64 {
    payload.len() as u64
}

pub fn main(rounds: u32, size: u32) -> u64 {
    let payload = loom::Bytes::from(vec![0x5a; size as usize]);
    let mut total = 0;
    for _ in 0..rounds {
        total += loom::isolated::call(ECHO, payload.clone()).expect("echo");
    }
    total
}
