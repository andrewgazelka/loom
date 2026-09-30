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
use std::sync::{Arc, Mutex, OnceLock};

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

type Slot = Arc<OnceLock<Result<Arc<TriBvh>, String>>>;

pub struct RgbHost {
    resident: Mutex<lru::LruCache<Handle, Slot>>,
}

impl Default for RgbHost {
    fn default() -> Self {
        Self {
            resident: Mutex::new(lru::LruCache::new(RESIDENT.try_into().expect("nonzero"))),
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
    /// The BVH of a handle's mesh: built once, then shared.
    fn bvh(&self, context: &KernelContext<'_>, handle: &Handle) -> Result<Arc<TriBvh>, String> {
        let slot = {
            let mut resident = self.resident.lock().expect("resident cache poisoned");
            resident
                .get_or_insert(*handle, || Arc::new(OnceLock::new()))
                .clone()
        };
        // The lock is released: a build of one mesh does not block queries on another,
        // and two callers of the same new handle build it once.
        slot.get_or_init(|| {
            let bytes = context
                .blob(handle)?
                .ok_or("the mesh handle names bytes this host never stored (loom::kernel::put first)")?;
            let values = f64s(&bytes, 9, "mesh")?;
            let triangles: Vec<[DVec3; 3]> = values
                .chunks_exact(9)
                .map(|t| [point(&t[0..3]), point(&t[3..6]), point(&t[6..9])])
                .collect();
            if triangles.is_empty() {
                return Err("the mesh has no triangles".into());
            }
            Ok(Arc::new(TriBvh::new(triangles)))
        })
        .clone()
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
        1
    }

    fn ops(&self) -> &[&'static str] {
        &["nearest", "ray", "overlapping", "contains"]
    }

    fn call(&self, context: &KernelContext<'_>, op: &str, args: &[&[u8]]) -> Result<Vec<u8>, String> {
        let handle: Handle = args
            .first()
            .and_then(|part| (*part).try_into().ok())
            .ok_or("the first argument is a 32-byte mesh handle")?;
        let bvh = self.bvh(context, &handle)?;
        match op {
            "nearest" => {
                let [_, points, radius] = args else {
                    return Err("nearest takes a handle, points and a radius".into());
                };
                let points = f64s(points, 3, "points")?;
                let radius = f64s(radius, 1, "radius")?;
                let radius = *radius.first().ok_or("nearest takes a radius")?;
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
                let [_, rays] = args else {
                    return Err("ray takes a handle and rays".into());
                };
                let rays = f64s(rays, 7, "rays")?;
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
                let [_, boxes] = args else {
                    return Err("overlapping takes a handle and boxes".into());
                };
                let boxes = f64s(boxes, 6, "boxes")?;
                let lists = Self::run(boxes.len() / 6, |i| {
                    let b = &boxes[i * 6..i * 6 + 6];
                    bvh.overlapping(&Aabb::new(point(&b[0..3]), point(&b[3..6])))
                });
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
                let [_, points] = args else {
                    return Err("contains takes a handle and points".into());
                };
                let points = f64s(points, 3, "points")?;
                Ok(Self::run(points.len() / 3, |i| u8::from(bvh.contains(point(&points[i * 3..i * 3 + 3])))))
            }
            other => Err(format!("rgb-host has no op {other:?}")),
        }
    }
}
