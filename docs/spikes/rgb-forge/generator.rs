// The wall_bracket generator of rgb's forge (crates/space-forge/src/gen/wall_bracket.rs) reduced to
// its pure geometry: the L section, extruded along x with a round bevel of 2 segments
// (crates/rgb-mesh/src/bevel.rs `prism_rings`/`build`, `ops::weld`), then triangulated as fans and
// written as little-endian f32 positions and u32 indices. glam vectors are replaced by plain
// [f64; N] arrays with glam's own operation order (normalize_or_zero multiplies by the reciprocal
// length), eyre by String errors. No dependencies, no unsafe: the same text is a Loom guest and a
// native crate.
use std::collections::BTreeMap;

#[cfg(feature = "libm")]
fn sin(x: f64) -> f64 { libm::sin(x) }
#[cfg(feature = "libm")]
fn cos(x: f64) -> f64 { libm::cos(x) }
#[cfg(feature = "libm")]
fn atan(x: f64) -> f64 { libm::atan(x) }
#[cfg(not(feature = "libm"))]
fn sin(x: f64) -> f64 { x.sin() }
#[cfg(not(feature = "libm"))]
fn cos(x: f64) -> f64 { x.cos() }
#[cfg(not(feature = "libm"))]
fn atan(x: f64) -> f64 { x.atan() }

type V2 = [f64; 2];
type V3 = [f64; 3];

fn add(a: V2, b: V2) -> V2 { [a[0] + b[0], a[1] + b[1]] }
fn sub(a: V2, b: V2) -> V2 { [a[0] - b[0], a[1] - b[1]] }
fn mul(a: V2, s: f64) -> V2 { [a[0] * s, a[1] * s] }
fn div(a: V2, s: f64) -> V2 { [a[0] / s, a[1] / s] }
fn dot(a: V2, b: V2) -> f64 { a[0] * b[0] + a[1] * b[1] }
fn perp_dot(a: V2, b: V2) -> f64 { a[0] * b[1] - a[1] * b[0] }
fn length(a: V2) -> f64 { dot(a, a).sqrt() }
fn normalize_or_zero(a: V2) -> V2 {
    let rcp = 1.0 / length(a);
    if rcp.is_finite() && rcp > 0.0 { mul(a, rcp) } else { [0.0, 0.0] }
}
fn left(v: V2) -> V2 { [-v[1], v[0]] }

const CAP: u32 = 1;
const SIDE: u32 = 0;
const STRIP: u32 = 2;

fn signed_area(poly: &[V2]) -> f64 {
    let n = poly.len();
    let mut s = 0.0;
    for i in 0..n {
        let p = poly[i];
        let q = poly[(i + 1) % n];
        s += p[0] * q[1] - q[0] * p[1];
    }
    0.5 * s
}

fn ccw(poly: &[V2]) -> Vec<V2> {
    let mut p = poly.to_vec();
    if signed_area(&p) < 0.0 {
        p.reverse();
    }
    p
}

#[derive(Clone, Copy)]
struct Corner {
    p: V2,
    a: V2,
    b: V2,
    tan_half: f64,
    convex: bool,
}

fn corners(poly: &[V2]) -> Result<Vec<Corner>, String> {
    let n = poly.len();
    if n < 3 {
        return Err(format!("an outline needs 3 points, got {n}"));
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let p = poly[i];
        let prev = poly[(i + n - 1) % n];
        let next = poly[(i + 1) % n];
        let a = normalize_or_zero(sub(p, prev));
        let b = normalize_or_zero(sub(next, p));
        if a == [0.0, 0.0] || b == [0.0, 0.0] {
            return Err(format!("repeated point at ({:.5}, {:.5})", p[0], p[1]));
        }
        let cross = perp_dot(a, b);
        let d = dot(a, b).clamp(-1.0, 1.0);
        if !(d > -1.0 + 1e-12) {
            return Err(format!("the outline folds back at ({:.5}, {:.5})", p[0], p[1]));
        }
        let tan_half = cross.abs() / (1.0 + d);
        out.push(Corner { p, a, b, tan_half, convex: cross > 0.0 });
    }
    Ok(out)
}

fn profile_points(segments: u32) -> Vec<V2> {
    let s = segments.max(1) as usize;
    (0..=s)
        .map(|k| {
            let t = core::f64::consts::FRAC_PI_2 * k as f64 / s as f64;
            [1.0 - sin(t), 1.0 - cos(t)]
        })
        .map(snap_ends)
        .collect()
}

fn snap_ends(p: V2) -> V2 {
    let f = |x: f64| {
        if x.abs() < 1e-14 { 0.0 } else if (x - 1.0).abs() < 1e-14 { 1.0 } else { x }
    };
    [f(p[0]), f(p[1])]
}

fn corner_arc(c: &Corner, q: V2, r: f64, segs: usize) -> Vec<V2> {
    if c.tan_half <= 1e-12 || r <= 0.0 {
        return vec![q; segs + 1];
    }
    let s = r * c.tan_half;
    let t1 = sub(q, mul(c.a, s));
    let t2 = add(q, mul(c.b, s));
    let sign = if c.convex { 1.0 } else { -1.0 };
    let centre = add(t1, mul(left(c.a), r * sign));
    let theta = 2.0 * atan(c.tan_half) * sign;
    let v = sub(t1, centre);
    (0..=segs)
        .map(|k| {
            if k == 0 {
                return t1;
            }
            if k == segs {
                return t2;
            }
            let ang = theta * k as f64 / segs as f64;
            let (sn, cs) = (sin(ang), cos(ang));
            add(centre, [v[0] * cs - v[1] * sn, v[0] * sn + v[1] * cs])
        })
        .collect()
}

fn mitre(c: &Corner) -> V2 {
    let na = left(c.a);
    let nb = left(c.b);
    div(add(na, nb), (1.0 + dot(na, nb)).max(1e-12))
}

fn radius_for(c: &Corner, setback: f64) -> f64 {
    if c.tan_half <= 1e-12 { 0.0 } else { setback / c.tan_half }
}

fn check_fits(poly: &[V2], setbacks: &[f64]) -> Result<(), String> {
    let n = poly.len();
    for i in 0..n {
        let j = (i + 1) % n;
        let len = length(sub(poly[j], poly[i]));
        if !(setbacks[i] + setbacks[j] <= len + 1e-12) {
            return Err(format!("the edge {i} is {len:.5} m, shorter than its two bevels"));
        }
    }
    Ok(())
}

fn check_ring(poly: &[V2], ring: &[V3], per: usize) -> Result<(), String> {
    let n = poly.len();
    for i in 0..n {
        let j = (i + 1) % n;
        let a = [ring[i * per + per - 1][0], ring[i * per + per - 1][1]];
        let b = [ring[j * per][0], ring[j * per][1]];
        if !(dot(sub(b, a), sub(poly[j], poly[i])) >= -1e-12) {
            return Err(format!("the bevel turns edge {i} inside out"));
        }
    }
    Ok(())
}

fn prism_rings(outline: &[V2], z0: f64, z1: f64, width: f64, segments: u32) -> Result<Vec<Vec<V3>>, String> {
    let poly = ccw(outline);
    let n = poly.len();
    let widths: Vec<f64> = vec![width; n];
    let (z0, z1) = if z0 <= z1 { (z0, z1) } else { (z1, z0) };
    let h = z1 - z0;
    for (i, &w) in widths.iter().enumerate() {
        if !(w >= 0.0) {
            return Err(format!("a negative bevel width {w} at corner {i}"));
        }
        if !(2.0 * w <= h + 1e-12) {
            return Err(format!("a {w:.5} m bevel does not fit a {h:.5} m tall prism"));
        }
    }
    let cs = corners(&poly)?;
    let setbacks: Vec<f64> = cs
        .iter()
        .zip(&widths)
        .map(|(c, &w)| if c.tan_half <= 1e-12 { 0.0 } else { w })
        .collect();
    check_fits(&poly, &setbacks)?;
    let segs = segments.max(1) as usize;
    let prof = profile_points(segments);
    let ring_at = |px: f64, py: f64, top: bool| -> Vec<V3> {
        let mut ring = Vec::with_capacity(n * (segs + 1));
        for (i, c) in cs.iter().enumerate() {
            let w = widths[i];
            let d = w * px;
            let z = if top { z1 - w * py } else { z0 + w * py };
            let q = add(c.p, mul(mitre(c), d));
            let r0 = radius_for(c, setbacks[i]);
            let r = if c.convex { r0 - d } else { r0 + d };
            for p in corner_arc(c, q, r, segs) {
                ring.push([p[0], p[1], z]);
            }
        }
        ring
    };
    let mut rings: Vec<Vec<V3>> = Vec::new();
    for u in &prof {
        rings.push(ring_at(u[0], u[1], false));
    }
    for u in prof.iter().rev() {
        rings.push(ring_at(u[0], u[1], true));
    }
    for ring in &rings {
        check_ring(&poly, ring, segs + 1)?;
    }
    Ok(rings)
}

struct Mesh {
    positions: Vec<V3>,
    faces: Vec<(Vec<u32>, u32)>,
}

fn build(rings: &[Vec<V3>], strip_rings: usize) -> Mesh {
    let mut m = Mesh { positions: Vec::new(), faces: Vec::new() };
    let mut vertex = |m: &mut Mesh, p: V3| -> u32 {
        m.positions.push(p);
        (m.positions.len() - 1) as u32
    };
    let ids: Vec<Vec<u32>> = rings
        .iter()
        .map(|r| r.iter().map(|&p| vertex(&mut m, p)).collect())
        .collect();
    let n = rings[0].len();
    let last = rings.len() - 1;
    for k in 0..last {
        let tag = if k + 1 == strip_rings { SIDE } else { STRIP };
        for i in 0..n {
            let j = (i + 1) % n;
            m.faces.push((vec![ids[k][i], ids[k][j], ids[k + 1][j], ids[k + 1][i]], tag));
        }
    }
    let first: Vec<u32> = ids[0].iter().rev().copied().collect();
    m.faces.push((first, CAP));
    m.faces.push((ids[last].clone(), CAP));
    weld(&mut m, 1e-10);
    m
}

fn weld(mesh: &mut Mesh, dist: f64) {
    let n = mesh.positions.len();
    if n == 0 {
        return;
    }
    let cell = dist.max(1e-12);
    let key = |p: V3| -> [i64; 3] {
        [(p[0] / cell).floor() as i64, (p[1] / cell).floor() as i64, (p[2] / cell).floor() as i64]
    };
    let mut grid: BTreeMap<[i64; 3], Vec<u32>> = BTreeMap::new();
    let mut target: Vec<u32> = (0..n as u32).collect();
    let d2 = dist * dist;
    for i in 0..n {
        let p = mesh.positions[i];
        let k = key(p);
        let mut found: Option<u32> = None;
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    if let Some(list) = grid.get(&[k[0] + dx, k[1] + dy, k[2] + dz]) {
                        for &j in list {
                            let q = mesh.positions[j as usize];
                            let d = [q[0] - p[0], q[1] - p[1], q[2] - p[2]];
                            let len2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
                            if len2 <= d2 && found.map_or(true, |f| j < f) {
                                found = Some(j);
                            }
                        }
                    }
                }
            }
        }
        match found {
            Some(j) => target[i] = j,
            None => grid.entry(k).or_default().push(i as u32),
        }
    }
    let faces = std::mem::take(&mut mesh.faces);
    for (verts, tag) in faces {
        let mut out: Vec<u32> = Vec::new();
        for &v in &verts {
            let t = target[v as usize];
            if out.last() == Some(&t) {
                continue;
            }
            out.push(t);
        }
        while out.len() > 1 && out.first() == out.last() {
            out.pop();
        }
        let mut unique = out.clone();
        unique.sort_unstable();
        unique.dedup();
        if unique.len() >= 3 && unique.len() == out.len() {
            mesh.faces.push((out, tag));
        }
    }
    // compact: drop vertices no face uses, renumbering in order
    let mut used = vec![false; n];
    for (verts, _) in &mesh.faces {
        for &v in verts {
            used[v as usize] = true;
        }
    }
    let mut new_index = vec![u32::MAX; n];
    let mut k = 0u32;
    for i in 0..n {
        if used[i] {
            new_index[i] = k;
            k += 1;
        }
    }
    if k as usize != n {
        mesh.positions = mesh.positions.iter().enumerate().filter(|(i, _)| used[*i]).map(|(_, p)| *p).collect();
        for (verts, _) in &mut mesh.faces {
            for v in verts.iter_mut() {
                *v = new_index[*v as usize];
            }
        }
    }
}

/// The bracket: L section in (y, z), extruded along x from -width/2 to width/2, bevel round of 2 segments.
pub fn bracket(width: f64, depth: f64, height: f64, thickness: f64, bevel: f64) -> Result<Vec<u8>, String> {
    if !(thickness < depth && thickness < height) {
        return Err("the plate is thicker than a leg".into());
    }
    let (t, d, h) = (thickness, depth, height);
    let section: Vec<V2> = vec![[0.0, -h], [t, -h], [t, -t], [d, -t], [d, 0.0], [0.0, 0.0]];
    let rings = prism_rings(&section, -width / 2.0, width / 2.0, bevel, 2)?;
    let mut m = build(&rings, 3);
    for p in &mut m.positions {
        *p = [p[2], p[0], p[1]];
    }
    // triangle fans, little-endian f32 positions then u32 indices
    let mut indices: Vec<u32> = Vec::new();
    for (verts, _) in &m.faces {
        for k in 1..verts.len() - 1 {
            indices.extend_from_slice(&[verts[0], verts[k], verts[k + 1]]);
        }
    }
    let mut out = Vec::with_capacity(8 + m.positions.len() * 12 + indices.len() * 4);
    out.extend_from_slice(&(m.positions.len() as u32).to_le_bytes());
    out.extend_from_slice(&(indices.len() as u32).to_le_bytes());
    for p in &m.positions {
        for c in p {
            out.extend_from_slice(&(*c as f32).to_le_bytes());
        }
    }
    for i in &indices {
        out.extend_from_slice(&i.to_le_bytes());
    }
    Ok(out)
}

/// Parameter set `i` of the spike's sweep: deterministic, always valid.
pub fn params(i: u32) -> [f64; 5] {
    let mut x = (i as u64).wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(0xD1B54A32D192ED03);
    let mut next = || {
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58476D1CE4E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D049BB133111EB);
        x ^= x >> 31;
        (x >> 11) as f64 / (1u64 << 53) as f64
    };
    let width = 0.08 + 0.12 * next();
    let depth = 0.05 + 0.07 * next();
    let height = 0.04 + 0.06 * next();
    let thickness = 0.003 + 0.005 * next();
    let bevel = thickness * (0.05 + 0.4 * next());
    [width, depth, height, thickness, bevel]
}

/// FNV-1a 64 over bytes, to compare meshes across builds without shipping them.
pub fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The hash of the bracket for each parameter set in `from..to`, 0 for a refused one.
pub fn sweep(from: u32, to: u32) -> Vec<u64> {
    (from..to)
        .map(|i| {
            let p = params(i);
            bracket(p[0], p[1], p[2], p[3], p[4]).map_or(0, |b| fnv(&b))
        })
        .collect()
}
