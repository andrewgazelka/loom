//! `rgb-host`: rgb's triangle-soup BVH queries as Loom host kernels.
//!
//! The BVH is rgb's own: `rgb-mesh/src/geom.rs` (`TriBvh`) and the tolerances it reads
//! (`rgb-mesh/src/check/tol.rs`) are included here unmodified, so a query through Loom
//! runs the same code, bit for bit, as the forge's native call. A crate inside rgb's
//! workspace would depend on `rgb-mesh` instead of including the file; nothing else changes.
//!
//! A mesh is a Loom content handle (the BLAKE3 hash of its bytes: `f64` little-endian,
//! nine per triangle, three corners of three coordinates). The first query on a handle
//! builds the BVH from the stored bytes; every later query on that handle, from any
//! guest, reuses the same immutable `Arc<TriBvh>`. Queries are batched, run in parallel
//! across the batch, and write into fixed output slots, so the result does not depend on
//! thread scheduling.
//!
//! Wire format (all little-endian; `n` is the batch size, derived from the buffer):
//!
//! | op | args | reply |
//! |---|---|---|
//! | `rgb-host.nearest` | handle, `f64[3n]` points, `f64` radius (`inf` for none) | `f64[3n]` closest points, `u32[n]` triangle |
//! | `rgb-host.ray` | handle, `f64[7n]` origin, direction, max | `f64[n]` t, `u32[n]` triangle |
//! | `rgb-host.overlapping` | handle, `f64[6n]` box min, max | `u32[n+1]` offsets, then `u32[m]` triangles |
//! | `rgb-host.contains` | handle, `f64[3n]` points | `u8[n]` |
//!
//! A miss is `u32::MAX` for the triangle, `0.0` for t and the point. Never a NaN.
use glam::DVec3;
use loom_rt::{Handle, HostKernel, KernelContext};
use rayon::prelude::*;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

#[allow(dead_code, clippy::all)]
#[path = "/Volumes/Projects/andrewgazelka/rgb/crates/rgb-mesh/src/geom.rs"]
pub mod geom;
/// The one module of `rgb-mesh`'s `check` that `geom.rs` reads.
pub mod check {
    #[allow(dead_code)]
    #[path = "/Volumes/Projects/andrewgazelka/rgb/crates/rgb-mesh/src/check/tol.rs"]
    pub mod tol;
}

pub use geom::TriBvh;
use geom::Aabb;

/// Resident BVHs: the immutable results of building from a handle's bytes.
const RESIDENT: usize = 16;
/// A batch smaller than this runs on the calling thread.
const PARALLEL_FROM: usize = 512;
/// Work per parallel task, so scheduling costs stay small against the queries.
const CHUNK: usize = 256;
/// Most queries one call may carry. A hostile mesh can make every query cost a full scan,
/// and the call cannot be interrupted, so the work per call is bounded up front.
const MAX_BATCH: usize = 1 << 20;
/// Most bytes an `overlapping` reply may hold. Each box can match every triangle, so
/// the reply is bounded by counting as the queries finish, before it is assembled.
const MAX_REPLY: usize = 64 << 20;
/// Bumped by hand when the wire format or the sentinels change. Everything else that can
/// change a result is hashed into [`RgbHost::version`].
const WIRE: u32 = 1;

type Slot = Arc<OnceLock<Result<Arc<TriBvh>, String>>>;

pub struct RgbHost {
    resident: Mutex<lru::LruCache<Handle, Slot>>,
    version: u32,
}

/// A digest of everything that decides a result: rgb's BVH and tolerance source, the
/// AABB crate, the resolved dependency versions (glam does the float math) and the wire
/// constant. Editing any of them changes the kernel version, hence the result-cache key,
/// so nothing stale survives an rgb change without anyone remembering to bump a number.
fn source_version() -> u32 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&WIRE.to_le_bytes());
    for part in [
        include_bytes!("/Volumes/Projects/andrewgazelka/rgb/crates/rgb-mesh/src/geom.rs").as_slice(),
        include_bytes!("/Volumes/Projects/andrewgazelka/rgb/crates/rgb-mesh/src/check/tol.rs"),
        include_bytes!("/Volumes/Projects/andrewgazelka/rgb/crates/rgb-bvh/src/lib.rs"),
        include_bytes!("../Cargo.lock"),
    ] {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    u32::from_le_bytes(digest.as_bytes()[..4].try_into().expect("four bytes"))
}

impl Default for RgbHost {
    fn default() -> Self {
        Self {
            resident: Mutex::new(lru::LruCache::new(RESIDENT.try_into().expect("nonzero"))),
            version: source_version(),
        }
    }
}

fn f64s(bytes: &[u8], per: usize, what: &str) -> Result<Vec<f64>, String> {
    if bytes.len() % (8 * per) != 0 {
        return Err(format!("{what}: {} bytes is not a whole number of {per}-value records", bytes.len()));
    }
    Ok(bytes
        .chunks_exact(8)
        .map(|word| f64::from_le_bytes(word.try_into().expect("chunks_exact(8)")))
        .collect())
}

fn point(values: &[f64]) -> DVec3 {
    DVec3::new(values[0], values[1], values[2])
}

fn put_f64(out: &mut [u8], index: usize, value: f64) {
    out[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut [u8], index: usize, value: u32) {
    out[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
}

impl RgbHost {
    #[doc(hidden)]
    pub fn version_for_tests(&self) -> u32 {
        self.version
    }

    /// The BVH of a handle's mesh: built once, then shared. A failure is not kept: whether
    /// a handle resolves depends on what this host has stored, which changes, so an error
    /// is the answer to this call only.
    fn bvh(&self, context: &KernelContext<'_>, handle: &Handle) -> Result<Arc<TriBvh>, String> {
        let slot = {
            let mut resident = self.resident.lock().expect("resident cache poisoned");
            resident
                .get_or_insert(*handle, || Arc::new(OnceLock::new()))
                .clone()
        };
        // The lock is released: a build of one mesh does not block queries on another,
        // and two callers of the same new handle build it once.
        let built = slot.get_or_init(|| {
            let bytes = context
                .blob(handle)?
                .ok_or("the mesh handle names bytes this host never stored (loom::kernel::put first)")?;
            if bytes.len() % 72 != 0 {
                return Err(format!(
                    "mesh: {} bytes is not a whole number of 72-byte triangles",
                    bytes.len()
                ));
            }
            let triangles: Vec<[DVec3; 3]> = bytes
                .chunks_exact(72)
                .map(|t| {
                    let at = |i: usize| f64::from_le_bytes(t[i * 8..i * 8 + 8].try_into().expect("8 bytes"));
                    let corner = |c: usize| DVec3::new(at(c * 3), at(c * 3 + 1), at(c * 3 + 2));
                    [corner(0), corner(1), corner(2)]
                })
                .collect();
            if triangles.is_empty() {
                return Err("the mesh has no triangles".into());
            }
            Ok(Arc::new(TriBvh::new(triangles)))
        });
        match built {
            Ok(bvh) => Ok(bvh.clone()),
            Err(error) => {
                let error = error.clone();
                let mut resident = self.resident.lock().expect("resident cache poisoned");
                if resident.peek(handle).is_some_and(|kept| Arc::ptr_eq(kept, &slot)) {
                    resident.pop(handle);
                }
                Err(error)
            }
        }
    }

    fn run<T: Send>(count: usize, work: impl Fn(usize) -> T + Sync + Send) -> Vec<T> {
        if count < PARALLEL_FROM {
            (0..count).map(work).collect()
        } else {
            (0..count).into_par_iter().with_min_len(CHUNK).map(work).collect()
        }
    }
}

impl HostKernel for RgbHost {
    fn family(&self) -> &str {
        "rgb-host"
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn ops(&self) -> &[&'static str] {
        &["nearest", "ray", "overlapping", "contains"]
    }

    fn call(&self, context: &KernelContext<'_>, op: &str, args: &[&[u8]]) -> Result<Vec<u8>, String> {
        let arity = match op {
            "nearest" => 3,
            "ray" | "overlapping" | "contains" => 2,
            other => return Err(format!("rgb-host has no op {other:?}")),
        };
        if args.len() != arity {
            return Err(format!("{op} takes {arity} arguments, got {}", args.len()));
        }
        let handle: Handle = args[0]
            .try_into()
            .map_err(|_| "the first argument is a 32-byte mesh handle".to_owned())?;
        // Sizes are checked before the mesh is resolved: a malformed call never triggers a build.
        let batch = |bytes: &[u8], per: usize, what: &str| -> Result<usize, String> {
            if bytes.len() % (8 * per) != 0 {
                return Err(format!("{what}: {} bytes is not a whole number of {per}-value records", bytes.len()));
            }
            let count = bytes.len() / (8 * per);
            if count > MAX_BATCH {
                return Err(format!("{what}: {count} queries in one call, at most {MAX_BATCH}"));
            }
            Ok(count)
        };
        match op {
            "nearest" => batch(args[1], 3, "points")?,
            "ray" => batch(args[1], 7, "rays")?,
            "overlapping" => batch(args[1], 6, "boxes")?,
            _ => batch(args[1], 3, "points")?,
        };
        let radius = if op == "nearest" {
            let bytes: [u8; 8] = args[2].try_into().map_err(|_| "radius is one f64 (8 bytes)".to_owned())?;
            let radius = f64::from_le_bytes(bytes);
            if radius.is_nan() || radius < 0.0 {
                return Err("radius must be a number >= 0 (inf for none)".into());
            }
            radius
        } else {
            0.0
        };
        let bvh = self.bvh(context, &handle)?;
        match op {
            "nearest" => {
                let points = f64s(args[1], 3, "points")?;
                let hits = Self::run(points.len() / 3, |i| {
                    let p = point(&points[i * 3..i * 3 + 3]);
                    if radius.is_infinite() { bvh.nearest(p) } else { bvh.nearest_within(p, radius) }
                });
                let n = hits.len();
                let mut out = vec![0u8; n * 24 + n * 4];
                for (i, hit) in hits.iter().enumerate() {
                    let (closest, triangle) = hit.map_or((DVec3::ZERO, u32::MAX), |h| h);
                    for (axis, value) in [closest.x, closest.y, closest.z].into_iter().enumerate() {
                        put_f64(&mut out, i * 3 + axis, value);
                    }
                    put_u32(&mut out[n * 24..], i, triangle);
                }
                Ok(out)
            }
            "ray" => {
                let rays = f64s(args[1], 7, "rays")?;
                let hits = Self::run(rays.len() / 7, |i| {
                    let r = &rays[i * 7..i * 7 + 7];
                    bvh.ray(point(&r[0..3]), point(&r[3..6]), r[6])
                });
                let n = hits.len();
                let mut out = vec![0u8; n * 8 + n * 4];
                for (i, hit) in hits.iter().enumerate() {
                    let (t, triangle) = hit.map_or((0.0, u32::MAX), |h| h);
                    put_f64(&mut out, i, t);
                    put_u32(&mut out[n * 8..], i, triangle);
                }
                Ok(out)
            }
            "overlapping" => {
                let boxes = f64s(args[1], 6, "boxes")?;
                // Count reply bytes as queries finish; once over the cap the rest are skipped.
                // An error is returned exactly when the true total exceeds the cap (skipping
                // starts only after the counter has passed it), so the outcome is deterministic.
                let used = AtomicUsize::new(0);
                let lists = Self::run(boxes.len() / 6, |i| {
                    if used.load(Ordering::Relaxed) > MAX_REPLY {
                        return Vec::new();
                    }
                    let b = &boxes[i * 6..i * 6 + 6];
                    let list = bvh.overlapping(&Aabb::new(point(&b[0..3]), point(&b[3..6])));
                    used.fetch_add(list.len() * 4, Ordering::Relaxed);
                    list
                });
                if used.load(Ordering::Relaxed) > MAX_REPLY {
                    return Err(format!("overlapping: the reply would exceed {MAX_REPLY} bytes; send fewer boxes"));
                }
                let n = lists.len();
                let total: usize = lists.iter().map(Vec::len).sum();
                let mut out = vec![0u8; (n + 1) * 4 + total * 4];
                let mut offset = 0u32;
                put_u32(&mut out, 0, 0);
                for (i, list) in lists.iter().enumerate() {
                    for (k, triangle) in list.iter().enumerate() {
                        put_u32(&mut out[(n + 1) * 4..], offset as usize + k, *triangle);
                    }
                    offset += list.len() as u32;
                    put_u32(&mut out, i + 1, offset);
                }
                Ok(out)
            }
            "contains" => {
                let points = f64s(args[1], 3, "points")?;
                Ok(Self::run(points.len() / 3, |i| u8::from(bvh.contains(point(&points[i * 3..i * 3 + 3])))))
            }
            _ => unreachable!("op checked above"),
        }
    }
}
