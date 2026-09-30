//! A native "cell": the same shape as a wasm one (typed arguments in as one DAG-CBOR array, one tagged
//! DAG-CBOR frame out) behind a C ABI instead of wasm exports. Entry glue is what `loom-build` would generate.
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};

#[repr(C)]
pub struct Frame {
    pub ptr: *mut u8,
    pub len: usize,
}

#[unsafe(no_mangle)]
pub extern "C" fn loom_dealloc(ptr: *mut u8, len: usize) {
    drop(unsafe { Vec::from_raw_parts(ptr, len, len) });
}

fn reply<T: Serialize>(out: *mut Frame, value: &T) -> i32 {
    let mut bytes = vec![0u8];
    bytes.extend(serde_ipld_dagcbor::to_vec(value).unwrap());
    bytes.shrink_to_fit();
    let frame = Frame { ptr: bytes.as_mut_ptr(), len: bytes.len() };
    std::mem::forget(bytes);
    unsafe { out.write(frame) };
    0
}

macro_rules! entry {
    ($export:ident, $args:ty, |$a:pat_param| $body:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $export(ptr: *const u8, len: usize, out: *mut Frame) -> i32 {
            let input = unsafe { std::slice::from_raw_parts(ptr, len) };
            let $a: $args = serde_ipld_dagcbor::from_slice(input).expect("decode");
            reply(out, &$body)
        }
    };
}

// 1. Pure compute, the same sieve the playground's `primes` preset runs as wasm.
entry!(loom_call_primes, (u32,), |(limit,)| {
    let limit = limit as usize;
    let mut prime = vec![true; limit + 1];
    prime[0] = false;
    prime[1] = false;
    let mut i = 2;
    while i * i <= limit {
        if prime[i] {
            for j in (i * i..=limit).step_by(i) {
                prime[j] = false;
            }
        }
        i += 1;
    }
    prime.iter().filter(|&&p| p).count() as u32
});

// 2. Bulk numbers as a CBOR array of floats: every element is a header plus 8 bytes, parsed one by one.
entry!(loom_call_sum_array, (Vec<f32>,), |(xs,)| xs.iter().map(|x| *x as f64).sum::<f64>());

// 3. The same numbers as one CBOR byte string of packed f32: borrowed from the input, no per-element parse.
struct Packed<'a>(&'a [u8]);
impl<'de> Deserialize<'de> for Packed<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Packed<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_borrowed_bytes<E: de::Error>(self, v: &'de [u8]) -> Result<Self::Value, E> {
                Ok(Packed(v))
            }
        }
        d.deserialize_bytes(V)
    }
}
entry!(loom_call_sum_packed, (Packed<'_>,), |(xs,)| {
    xs.0.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap()) as f64).sum::<f64>()
});

// 4. A scene like the spirograph's: 424 (kind, coords, colour) commands out, no effect dispatch.
entry!(loom_call_scene, (u32,), |(ms,)| {
    let t = ms as f32 / 1000.0;
    let (big, small, pen) = (0.72_f32, 0.27_f32, 0.42_f32);
    let point = |a: f32| {
        let k = (big - small) / small;
        [(big - small) * a.cos() + pen * (k * a).cos(), (big - small) * a.sin() - pen * (k * a).sin()]
    };
    let now = t * 0.9;
    let mut scene: Vec<(u8, Vec<f32>, [u8; 3])> = Vec::new();
    for s in 0..420 {
        let a = now - s as f32 * 0.035;
        let (p, q) = (point(a), point(a - 0.035));
        scene.push((0, vec![p[0], p[1], q[0], q[1]], [200, 180, 255]));
    }
    scene.push((1, vec![0.0, 0.0, big], [70, 76, 110]));
    scene.push((1, vec![0.1, 0.1, small], [70, 76, 110]));
    scene.push((1, vec![0.2, 0.2, 0.03], [255, 158, 100]));
    scene.push((0, vec![0.0, 0.0, 0.5, 0.5], [255, 158, 100]));
    scene
});
