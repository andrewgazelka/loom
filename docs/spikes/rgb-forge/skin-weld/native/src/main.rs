//! Reference side of the skin-weld end-to-end check: build the engine's synthetic neck (the constants and
//! builders of crates/skin-weld/src/tests.rs, copied), run the in-process `weld`, and write the inputs and
//! the expected output in the packed container format the Loom guest reads and writes.
use skin_weld::{Params, Skin, Weld, weld};
use std::path::PathBuf;

const AROUND: usize = 48;
const ROWS: usize = 40;
const DY: f64 = 0.006;
const RADIUS: f64 = 0.055;
const BODY_TOP: usize = 24;
const HEAD_BOTTOM: usize = 16;

fn point(i: usize, j: usize, tuck: f64) -> [f32; 3] {
    let a = core::f64::consts::TAU * i as f64 / AROUND as f64;
    let r = RADIUS - tuck;
    [(r * a.cos()) as f32, (j as f64 * DY) as f32, (r * a.sin()) as f32]
}

fn tube(rows: core::ops::RangeInclusive<usize>, cap_top: bool, tuck: impl Fn(usize) -> f64) -> Skin {
    let mut s = Skin::default();
    let (lo, hi) = (*rows.start(), *rows.end());
    for j in lo..=hi {
        for i in 0..=AROUND {
            s.positions.push(point(i % AROUND, j, tuck(j)));
            s.uvs.push([i as f32 / AROUND as f32, j as f32 / ROWS as f32]);
            s.joints.push([0, 1, 0, 0]);
            s.weights.push([1.0, 0.0, 0.0, 0.0]);
            let a = core::f64::consts::TAU * i as f64 / AROUND as f64;
            s.normals.push([a.cos() as f32, 0.0, a.sin() as f32]);
        }
    }
    let at = |i: usize, j: usize| ((j - lo) * (AROUND + 1) + i) as u32;
    for j in lo..hi {
        for i in 0..AROUND {
            let (a, b, c, d) = (at(i, j), at(i + 1, j), at(i + 1, j + 1), at(i, j + 1));
            s.triangles.push([a, c, b]);
            s.triangles.push([a, d, c]);
        }
    }
    let cap_row = if cap_top { hi } else { lo };
    let centre = s.positions.len() as u32;
    let y = (cap_row as f64 * DY + if cap_top { 0.03 } else { -0.03 }) as f32;
    s.positions.push([0.0, y, 0.0]);
    s.uvs.push([0.5, if cap_top { 1.0 } else { 0.0 }]);
    s.joints.push([1, 0, 0, 0]);
    s.weights.push([1.0, 0.0, 0.0, 0.0]);
    s.normals.push([0.0, if cap_top { 1.0 } else { -1.0 }, 0.0]);
    for i in 0..AROUND {
        let (a, b) = (at(i, cap_row), at(i + 1, cap_row));
        s.triangles.push(if cap_top { [b, a, centre] } else { [a, b, centre] });
    }
    s
}

fn head() -> Skin {
    let mut h = tube(HEAD_BOTTOM..=ROWS - 1, true, |j| {
        if j < BODY_TOP { 0.001 * (BODY_TOP - j) as f64 } else { 0.0 }
    });
    h.targets = vec![vec![[0.001, 0.0, 0.0]; h.len()]];
    h
}

fn body() -> Skin {
    tube(0..=BODY_TOP, false, |_| 0.0)
}

// ---- container: u32 magic, u32 sections, then u64 byte length per section, then each section padded to 8.
fn bytes_of<T>(values: &[T]) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), std::mem::size_of_val(values)) }.to_vec()
}
fn pack(magic: u32, sections: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(magic.to_le_bytes());
    out.extend((sections.len() as u32).to_le_bytes());
    for section in sections {
        out.extend((section.len() as u64).to_le_bytes());
    }
    for section in sections {
        out.extend(section);
        while out.len() % 8 != 0 {
            out.push(0);
        }
    }
    out
}
fn skin_sections(s: &Skin) -> Vec<Vec<u8>> {
    let mut v = vec![
        bytes_of(&s.positions),
        bytes_of(&s.normals),
        bytes_of(&s.uvs),
        bytes_of(&s.joints),
        bytes_of(&s.weights),
        bytes_of(&s.triangles),
    ];
    for t in &s.targets {
        v.push(bytes_of(t));
    }
    v
}
const SKIN: u32 = 0x314e_4b53; // "SKN1"
const WELD: u32 = 0x3144_4c57; // "WLD1"
fn pack_skin(s: &Skin) -> Vec<u8> {
    pack(SKIN, &skin_sections(s))
}
fn opt(v: &[Option<u32>]) -> Vec<u8> {
    bytes_of(&v.iter().map(|x| x.unwrap_or(u32::MAX)).collect::<Vec<_>>())
}
fn pack_weld(w: &Weld) -> Vec<u8> {
    let r = &w.report;
    let report = [
        r.seam_ring as f64, r.seam_nodes as f64, r.gap_m[0], r.gap_m[1], r.gap_m[2], r.crossing_deg[0],
        r.crossing_deg[1], r.collar_m, r.head_moved_m, r.head_blended as f64, r.head_dropped_vertices as f64,
        r.head_dropped_triangles as f64, r.body_dropped_vertices as f64, r.body_dropped_triangles as f64,
        r.strip_triangles as f64, r.head_stray_nodes as f64, r.head_peel_rounds as f64,
    ];
    pack(
        WELD,
        &[
            pack_skin(&w.head),
            pack_skin(&w.body),
            opt(&w.head_vertices),
            opt(&w.head_triangles),
            opt(&w.body_vertices),
            opt(&w.body_triangles),
            bytes_of(&report),
        ],
    )
}

fn read<T: Copy>(path: &std::path::Path) -> Vec<T> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let size = std::mem::size_of::<T>();
    assert_eq!(bytes.len() % size, 0, "{}", path.display());
    let mut out: Vec<T> = Vec::with_capacity(bytes.len() / size);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.as_mut_ptr().cast::<u8>(), bytes.len());
        out.set_len(bytes.len() / size);
    }
    out
}

fn dump_skin(dir: &std::path::Path, name: &str) -> Skin {
    let f = |part: &str| dir.join(format!("{name}.{part}.bin"));
    Skin {
        positions: read(&f("positions")),
        uvs: read(&f("uvs")),
        normals: read(&f("normals")),
        joints: read(&f("joints")),
        weights: read(&f("weights")),
        triangles: read(&f("triangles")),
        targets: Vec::new(),
    }
}

/// Real-size mode: load the engine's dump, run the in-process weld several times, write the packed containers.
fn from_dump(dump: &std::path::Path, out: &std::path::Path) {
    std::fs::create_dir_all(out).unwrap();
    let (h, b) = (dump_skin(dump, "head"), dump_skin(dump, "body"));
    let p: Vec<f64> = read(&dump.join("params.f64.bin"));
    let params = Params {
        uv_eps: p[0], twin_max_m: p[1], max_ring: p[2] as u32, blend_m: p[3], shape_fade_m: p[4],
        normal_fade_m: p[5], bridge_margin_m: p[6], bridge_cut_m: p[7], bridge_clear_m: p[8],
    };
    let mut times = Vec::new();
    let mut welded = None;
    for _ in 0..7 {
        let t = std::time::Instant::now();
        welded = Some(weld(&h, &b, &params).expect("weld"));
        times.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let w = welded.unwrap();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    std::fs::write(out.join("head.skin"), pack_skin(&h)).unwrap();
    std::fs::write(out.join("body.skin"), pack_skin(&b)).unwrap();
    std::fs::write(out.join("expected.weld"), pack_weld(&w)).unwrap();
    std::fs::write(out.join("params.json"), serde_list(&p)).unwrap();
    // The engine's own output, to prove this harness reproduces it before anything is compared with Loom.
    let (eh, eb) = (dump_skin(dump, "weld_out.head"), dump_skin(dump, "weld_out.body"));
    let same = |a: &Skin, b: &Skin| a.positions == b.positions && a.uvs == b.uvs && a.normals == b.normals
        && a.joints == b.joints && a.weights == b.weights && a.triangles == b.triangles;
    println!(
        "head {} verts {} tris, body {} verts {} tris -> welded head {} / body {}; native weld median {:.1} ms (min {:.1}); harness output equals the engine's dump: head {} body {}",
        h.len(), h.triangles.len(), b.len(), b.triangles.len(), w.head.len(), w.body.len(), times[times.len() / 2], times[0],
        same(&w.head, &eh), same(&w.body, &eb)
    );
}

fn main() {
    if let (Some(dump), Some(out)) = (std::env::args().nth(2), std::env::args().nth(3)) {
        if std::env::args().nth(1).as_deref() == Some("--dump") {
            from_dump(std::path::Path::new(&dump), std::path::Path::new(&out));
            return;
        }
    }
    let out = PathBuf::from(std::env::args().nth(1).expect("output directory"));
    std::fs::create_dir_all(&out).unwrap();
    let params = Params { blend_m: 0.005, ..Params::default() };
    let (h, b) = (head(), body());
    let started = std::time::Instant::now();
    let welded = weld(&h, &b, &params).expect("weld");
    let native_us = started.elapsed().as_secs_f64() * 1e6;
    std::fs::write(out.join("head.skin"), pack_skin(&h)).unwrap();
    std::fs::write(out.join("body.skin"), pack_skin(&b)).unwrap();
    std::fs::write(out.join("expected.weld"), pack_weld(&welded)).unwrap();
    let p = &params;
    let list = [p.uv_eps, p.twin_max_m, p.max_ring as f64, p.blend_m, p.shape_fade_m, p.normal_fade_m, p.bridge_margin_m, p.bridge_cut_m, p.bridge_clear_m];
    std::fs::write(out.join("params.json"), serde_list(&list)).unwrap();
    println!(
        "head {} verts {} tris, body {} verts {} tris -> welded head {} / body {} verts, strip {}, native weld {:.0} us",
        h.len(), h.triangles.len(), b.len(), b.triangles.len(), welded.head.len(), welded.body.len(), welded.report.strip_triangles, native_us
    );
}
fn serde_list(v: &[f64]) -> String {
    format!("[{}]", v.iter().map(|x| format!("{x:?}")).collect::<Vec<_>>().join(","))
}
