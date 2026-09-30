//! The kernel against rgb's own TriBvh called directly: bit-equal results, every op,
//! every batch-size boundary, and the error paths.
use loom_rt::Runtime;
use loom_store::Store;
use rgb_host_kernels::{geom::Aabb, RgbHost, TriBvh};
use glam::DVec3;
use std::sync::Arc;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }
    fn between(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next()
    }
    fn direction(&mut self) -> DVec3 {
        loop {
            let v = DVec3::new(self.between(-1.0, 1.0), self.between(-1.0, 1.0), self.between(-1.0, 1.0));
            if v.length_squared() > 1e-6 && v.length_squared() <= 1.0 {
                return v.normalize();
            }
        }
    }
}

/// A bumpy UV sphere of about `24 * rings` triangles, deterministic.
fn sphere(segments: usize, rings: usize) -> Vec<[DVec3; 3]> {
    let mut rng = Lcg(7);
    let radius = |theta: f64, phi: f64| 1.0 + 0.08 * (5.0 * theta).sin() * (3.0 * phi).cos() + 0.002 * rng_bump(theta, phi);
    fn rng_bump(a: f64, b: f64) -> f64 {
        (a * 12.9898 + b * 78.233).sin()
    }
    let _ = &mut rng;
    let vertex = |i: usize, j: usize| {
        let theta = std::f64::consts::PI * j as f64 / rings as f64;
        let phi = 2.0 * std::f64::consts::PI * (i % segments) as f64 / segments as f64;
        let r = radius(theta, phi);
        DVec3::new(r * theta.sin() * phi.cos(), r * theta.sin() * phi.sin(), r * theta.cos())
    };
    let mut triangles = Vec::new();
    for j in 0..rings {
        for i in 0..segments {
            let (a, b, c, d) = (vertex(i, j), vertex(i + 1, j), vertex(i + 1, j + 1), vertex(i, j + 1));
            if j > 0 {
                triangles.push([a, b, d]);
            }
            if j + 1 < rings {
                triangles.push([b, c, d]);
            }
        }
    }
    triangles
}

fn bytes_of(triangles: &[[DVec3; 3]]) -> Vec<u8> {
    triangles
        .iter()
        .flat_map(|t| t.iter().flat_map(|p| [p.x, p.y, p.z]))
        .flat_map(f64::to_le_bytes)
        .collect()
}

fn f64_bytes(values: impl IntoIterator<Item = f64>) -> Vec<u8> {
    values.into_iter().flat_map(f64::to_le_bytes).collect()
}

fn setup() -> (Runtime, Vec<u8>, TriBvh) {
    let triangles = sphere(128, 96);
    let runtime = Runtime::new(Store::memory().unwrap()).unwrap();
    runtime.register_kernel(Arc::new(RgbHost::default())).unwrap();
    let handle = runtime.call_kernel("loom.put", &[&bytes_of(&triangles)]).unwrap();
    (runtime, handle, TriBvh::new(triangles))
}

fn u32s(bytes: &[u8]) -> Vec<u32> {
    bytes.chunks_exact(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect()
}

fn f64s(bytes: &[u8]) -> Vec<f64> {
    bytes.chunks_exact(8).map(|w| f64::from_le_bytes(w.try_into().unwrap())).collect()
}

#[test]
fn every_op_is_bit_equal_to_the_native_bvh_at_every_batch_size() {
    let (runtime, handle, native) = setup();
    let mut rng = Lcg(42);
    for n in [1usize, 2, 100, 511, 512, 513, 5000] {
        let points: Vec<DVec3> = (0..n).map(|_| DVec3::new(rng.between(-1.5, 1.5), rng.between(-1.5, 1.5), rng.between(-1.5, 1.5))).collect();
        let point_bytes = f64_bytes(points.iter().flat_map(|p| [p.x, p.y, p.z]));

        // nearest, unbounded and within a radius
        for radius in [f64::INFINITY, 0.05] {
            let reply = runtime.call_kernel("rgb-host.nearest", &[&handle, &point_bytes, &radius.to_le_bytes()]).unwrap();
            let (closest, triangles) = reply.split_at(n * 24);
            let (closest, triangles) = (f64s(closest), u32s(triangles));
            for (i, p) in points.iter().enumerate() {
                let want = if radius.is_infinite() { native.nearest(*p) } else { native.nearest_within(*p, radius) };
                match want {
                    Some((c, t)) => {
                        assert_eq!(triangles[i], t, "n={n} nearest {i}");
                        assert_eq!([closest[i * 3].to_bits(), closest[i * 3 + 1].to_bits(), closest[i * 3 + 2].to_bits()],
                                   [c.x.to_bits(), c.y.to_bits(), c.z.to_bits()], "n={n} nearest point {i}");
                    }
                    None => assert_eq!(triangles[i], u32::MAX, "n={n} nearest miss {i}"),
                }
            }
        }

        // rays aimed from outside at random inside points
        let rays: Vec<(DVec3, DVec3, f64)> = (0..n)
            .map(|_| {
                let origin = rng.direction() * 3.0;
                let target = DVec3::new(rng.between(-0.6, 0.6), rng.between(-0.6, 0.6), rng.between(-0.6, 0.6));
                (origin, (target - origin).normalize(), 10.0)
            })
            .collect();
        let ray_bytes = f64_bytes(rays.iter().flat_map(|(o, d, m)| [o.x, o.y, o.z, d.x, d.y, d.z, *m]));
        let reply = runtime.call_kernel("rgb-host.ray", &[&handle, &ray_bytes]).unwrap();
        let (times, triangles) = reply.split_at(n * 8);
        let (times, triangles) = (f64s(times), u32s(triangles));
        let mut hits = 0;
        for (i, (o, d, m)) in rays.iter().enumerate() {
            match native.ray(*o, *d, *m) {
                Some((t, tri)) => {
                    hits += 1;
                    assert_eq!((times[i].to_bits(), triangles[i]), (t.to_bits(), tri), "n={n} ray {i}");
                }
                None => assert_eq!((times[i], triangles[i]), (0.0, u32::MAX), "n={n} ray miss {i}"),
            }
        }
        assert!(hits * 2 > n, "the rays hit the sphere ({hits} of {n})");

        // boxes: CSR
        let boxes: Vec<(DVec3, DVec3)> = (0..n)
            .map(|_| {
                let c = DVec3::new(rng.between(-1.2, 1.2), rng.between(-1.2, 1.2), rng.between(-1.2, 1.2));
                let h = DVec3::splat(rng.between(0.01, 0.3));
                (c - h, c + h)
            })
            .collect();
        let box_bytes = f64_bytes(boxes.iter().flat_map(|(lo, hi)| [lo.x, lo.y, lo.z, hi.x, hi.y, hi.z]));
        let reply = runtime.call_kernel("rgb-host.overlapping", &[&handle, &box_bytes]).unwrap();
        let words = u32s(&reply);
        let (offsets, indices) = words.split_at(n + 1);
        assert_eq!(offsets[0], 0);
        for (i, (lo, hi)) in boxes.iter().enumerate() {
            let want = native.overlapping(&Aabb::new(*lo, *hi));
            assert_eq!(&indices[offsets[i] as usize..offsets[i + 1] as usize], &want[..], "n={n} box {i}");
        }
        assert_eq!(offsets[n] as usize, indices.len());

        // contains
        let reply = runtime.call_kernel("rgb-host.contains", &[&handle, &point_bytes]).unwrap();
        assert_eq!(reply.len(), n);
        for (i, p) in points.iter().enumerate() {
            assert_eq!(reply[i] == 1, native.contains(*p), "n={n} contains {i}");
        }
    }
}

#[test]
fn a_repeated_call_returns_identical_bytes_whatever_the_thread_schedule() {
    let (runtime, handle, _) = setup();
    let mut rng = Lcg(9);
    let points = f64_bytes((0..3 * 4000).map(|_| rng.between(-1.5, 1.5)));
    let first = runtime.call_kernel("rgb-host.contains", &[&handle, &points]).unwrap();
    for _ in 0..5 {
        assert_eq!(runtime.call_kernel("rgb-host.contains", &[&handle, &points]).unwrap(), first);
    }
    let inf = f64::INFINITY.to_le_bytes();
    let nearest = runtime.call_kernel("rgb-host.nearest", &[&handle, &points, &inf]).unwrap();
    for _ in 0..5 {
        assert_eq!(runtime.call_kernel("rgb-host.nearest", &[&handle, &points, &inf]).unwrap(), nearest);
    }
}

#[test]
fn bad_input_is_an_error_message_not_a_crash_and_the_host_keeps_working() {
    let (runtime, handle, _) = setup();
    let unknown = [3u8; 32];
    let error = runtime.call_kernel("rgb-host.contains", &[&unknown, &f64_bytes([0.0; 3])]).unwrap_err();
    assert!(error.contains("never stored"), "{error}");
    assert!(runtime.call_kernel("rgb-host.contains", &[&handle[..31], &f64_bytes([0.0; 3])]).unwrap_err().contains("32-byte"));
    assert!(runtime.call_kernel("rgb-host.ray", &[&handle, &[0u8; 55]]).unwrap_err().contains("whole number"));
    assert!(runtime.call_kernel("rgb-host.nearest", &[&handle, &f64_bytes([0.0; 3])]).is_err());
    assert!(runtime.call_kernel("rgb-host.contains", &[&handle, &f64_bytes([0.0; 3])]).is_ok());
    // A handle whose bytes are not a whole number of triangles.
    let odd = runtime.call_kernel("loom.put", &[&[1u8; 100]]).unwrap();
    assert!(runtime.call_kernel("rgb-host.contains", &[&odd, &f64_bytes([0.0; 3])]).unwrap_err().contains("mesh"));
    let empty = runtime.call_kernel("loom.put", &[&[]]).unwrap();
    assert!(runtime.call_kernel("rgb-host.contains", &[&empty, &f64_bytes([0.0; 3])]).unwrap_err().contains("no triangles"));
}

#[test]
fn a_failed_lookup_is_not_remembered_once_the_bytes_arrive() {
    let (runtime, _, _) = setup();
    let mesh = bytes_of(&sphere(16, 8));
    let future = *blake3::hash(&mesh).as_bytes();
    let point = f64_bytes([0.0; 3]);
    assert!(runtime.call_kernel("rgb-host.contains", &[&future, &point]).unwrap_err().contains("never stored"));
    let stored = runtime.call_kernel("loom.put", &[&mesh]).unwrap();
    assert_eq!(stored, future);
    assert!(runtime.call_kernel("rgb-host.contains", &[&future, &point]).is_ok());
}

#[test]
fn a_radius_that_is_not_a_distance_is_an_error_and_extra_bytes_are_refused() {
    let (runtime, handle, _) = setup();
    let point = f64_bytes([0.0; 3]);
    for bad in [f64::NAN, -1.0, f64::NEG_INFINITY] {
        let error = runtime.call_kernel("rgb-host.nearest", &[&handle, &point, &bad.to_le_bytes()]).unwrap_err();
        assert!(error.contains("radius"), "{bad}: {error}");
    }
    assert!(runtime.call_kernel("rgb-host.nearest", &[&handle, &point, &f64_bytes([1.0, 2.0])]).is_err());
    assert!(runtime.call_kernel("rgb-host.nearest", &[&handle, &point, &0.0f64.to_le_bytes()]).is_ok());
}

#[test]
fn a_malformed_call_never_builds_the_mesh_and_an_empty_batch_is_empty() {
    let (runtime, handle, _) = setup();
    let unknown = [9u8; 32];
    // Arity and sizes are checked first: this error is about the shape, not the missing mesh.
    let error = runtime.call_kernel("rgb-host.ray", &[&unknown, &[0u8; 7]]).unwrap_err();
    assert!(error.contains("whole number"), "{error}");
    assert!(runtime.call_kernel("rgb-host.nope", &[&unknown]).unwrap_err().contains("no kernel op"));
    assert!(runtime.call_kernel("rgb-host.contains", &[&handle, &[], &[]]).unwrap_err().contains("takes 2"));
    assert_eq!(runtime.call_kernel("rgb-host.contains", &[&handle, &[]]).unwrap(), Vec::<u8>::new());
    assert_eq!(runtime.call_kernel("rgb-host.overlapping", &[&handle, &[]]).unwrap(), vec![0, 0, 0, 0]);
}

#[test]
fn the_overlapping_reply_is_bounded_and_the_version_follows_the_source() {
    let (runtime, handle, _) = setup();
    // 24k triangles per box, 4 bytes each: a few hundred boxes over the whole sphere is under the cap,
    // a hundred thousand is not.
    let whole = [-2.0, -2.0, -2.0, 2.0, 2.0, 2.0];
    let many = f64_bytes(std::iter::repeat_n(whole, 100_000).flatten());
    let error = runtime.call_kernel("rgb-host.overlapping", &[&handle, &many]).unwrap_err();
    assert!(error.contains("exceed"), "{error}");
    let few = f64_bytes(std::iter::repeat_n(whole, 4).flatten());
    assert!(runtime.call_kernel("rgb-host.overlapping", &[&handle, &few]).is_ok());
    // The version is a digest, not a literal: it changes the kernel fingerprint.
    assert_ne!(RgbHost::default().version_for_tests(), 1);
}
