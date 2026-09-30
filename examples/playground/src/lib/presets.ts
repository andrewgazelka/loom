export type Preset = {
  name: string;
  /** One line: what this shows and what to try. */
  note: string;
  cell: string;
  /** "value": the cell's last expression is shown as the result. "scene": the cell draws with effects. */
  kind: "value" | "scene";
  glam?: boolean;
  animate?: boolean;
  lib?: { name: string; source: string };
};

export const manifest = `[package]
name = "cell"
version = "0.1.0"
edition = "2024"
[dependencies]
glam = "=0.33.10"
`;

export const lock = `version = 4

[[package]]
name = "glam"
version = "0.33.10"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "928452f9c953e142b2f0973e4bd5f34445fcaa8069556ea97a3c9d34d15a4cf8"
`;

// The drawing effects and their handler. Appended to every "scene" cell; the canvas.rs tab shows it.
export const prelude = `// Drawing is an effect. canvas2d and canvas3d only *perform* labelled effects
// with typed payloads (tuples and arrays, no JSON); \`collect\` is the handler
// that decides what they mean: here, append to a scene and resume.
use loom::{Continuation, Effect, Reply, Value};

type Rgb = [u8; 3];
/// One drawing command: (kind, coordinates, colour). Kinds: see \`KINDS\`.
type Scene = Vec<(u8, Vec<f32>, Rgb)>;

const KINDS: [&str; 4] = ["canvas2d.line", "canvas2d.circle", "canvas3d.line", "canvas3d.tri"];

mod canvas2d {
    use super::*;
    pub fn line(a: [f32; 2], b: [f32; 2], color: Rgb) {
        let _ = loom::perform::<()>("canvas2d.line", (a, b, color));
    }
    pub fn circle(at: [f32; 2], radius: f32, color: Rgb) {
        let _ = loom::perform::<()>("canvas2d.circle", (at, radius, color));
    }
}

mod canvas3d {
    use super::*;
    pub fn line(a: [f32; 3], b: [f32; 3], color: Rgb) {
        let _ = loom::perform::<()>("canvas3d.line", (a, b, color));
    }
    pub fn tri(a: [f32; 3], b: [f32; 3], c: [f32; 3], color: Rgb) {
        let _ = loom::perform::<()>("canvas3d.tri", (a, b, c, color));
    }
}

/// The handler. Runs \`body\`; every canvas effect it performs lands here.
fn collect(body: impl FnOnce()) -> Scene {
    let mut scene = Scene::new();
    loom::handle(KINDS, |e: Effect, _k: Continuation| {
        let kind = KINDS.iter().position(|k| *k == e.name).unwrap_or(0) as u8;
        let (coords, color) = match kind {
            0 => { let (a, b, c): ([f32; 2], [f32; 2], Rgb) = e.arg().unwrap(); ([a[0], a[1], b[0], b[1]].to_vec(), c) }
            1 => { let (at, r, c): ([f32; 2], f32, Rgb) = e.arg().unwrap(); ([at[0], at[1], r].to_vec(), c) }
            2 => { let (a, b, c): ([f32; 3], [f32; 3], Rgb) = e.arg().unwrap(); ([a, b].concat(), c) }
            _ => { let (a, b, c, k): ([f32; 3], [f32; 3], [f32; 3], Rgb) = e.arg().unwrap(); ([a, b, c].concat(), k) }
        };
        scene.push((kind, coords, color));
        Reply::Resume(Value::Null)
    }, body).expect("handler failed");
    scene
}

/// The entry the page calls: run the cell's \`scene\` under the handler.
pub fn frame(ms: u32) -> Scene {
    collect(|| scene(ms))
}`;

export const presets: Preset[] = [
  {
    name: "hello",
    kind: "value",
    note: "A cell is a block of Rust; its last expression is the result. Try changing 20.",
    cell: `let mut fib = vec![0u64, 1];
for i in 2..20 {
    let next = fib[i - 1] + fib[i - 2];
    fib.push(next);
}
fib`,
  },
  {
    name: "primes",
    kind: "value",
    note: "Real compute, compiled to WebAssembly. Try 2_000_000 and watch the run time.",
    cell: `let limit = 200_000usize;
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
prime.iter().filter(|&&p| p).count()`,
  },
  {
    name: "spirograph",
    kind: "scene",
    animate: true,
    note: "Drawing is an effect: the cell asks for lines, a handler collects them, the page paints. Edit a number.",
    cell: `use std::f32::consts::TAU;

// Draw with effects: canvas2d::line and canvas2d::circle only ask for a
// drawing. \`ms\` is the page clock, so the picture moves.
fn scene(ms: u32) {
    let t = ms as f32 / 1000.0;
    let (big, small, pen) = (0.72_f32, 0.27_f32, 0.42_f32);
    let point = |a: f32| {
        let k = (big - small) / small;
        [
            (big - small) * a.cos() + pen * (k * a).cos(),
            (big - small) * a.sin() - pen * (k * a).sin(),
        ]
    };
    let now = t * 0.9;
    let steps = 420;
    for s in 0..steps {
        let a = now - s as f32 * 0.035;
        let fade = 1.0 - s as f32 / steps as f32;
        let c = [(120.0 + 135.0 * fade) as u8, (90.0 + 120.0 * fade) as u8, 255];
        canvas2d::line(point(a), point(a - 0.035), c);
    }
    let hub = [(big - small) * now.cos(), (big - small) * now.sin()];
    canvas2d::circle([0.0, 0.0], big, [70, 76, 110]);
    canvas2d::circle(hub, small, [70, 76, 110]);
    canvas2d::circle(point(now), 0.03, [255, 158, 100]);
    canvas2d::line(hub, point(now), [255, 158, 100]);
    let _ = TAU;
}`,
  },
  {
    name: "terrain",
    kind: "scene",
    note: "Same idea in 3D. Drag to orbit, scroll to zoom. Rust builds the mesh; the page only rotates it.",
    cell: `// A height field: one point per grid cell, two triangles per square.
fn scene(_ms: u32) {
    let n = 36;
    let height = |x: f32, z: f32| {
        let r = (x * x + z * z).sqrt();
        0.42 * (r * 5.0).cos() / (1.0 + 2.2 * r)
            + 0.12 * (x * 3.0).sin() * (z * 2.0).cos()
    };
    let colour = |h: f32| {
        let k = ((h + 0.25) / 0.6).clamp(0.0, 1.0);
        [(50.0 + 200.0 * k) as u8, (90.0 + 110.0 * k) as u8, (220.0 - 130.0 * k) as u8]
    };
    let at = |i: usize, j: usize| {
        let x = i as f32 / n as f32 * 2.4 - 1.2;
        let z = j as f32 / n as f32 * 2.4 - 1.2;
        [x, height(x, z), z]
    };
    for i in 0..n {
        for j in 0..n {
            let (a, b, c, d) = (at(i, j), at(i + 1, j), at(i + 1, j + 1), at(i, j + 1));
            let mean = (a[1] + b[1] + c[1] + d[1]) / 4.0;
            canvas3d::tri(a, b, c, colour(mean));
            canvas3d::tri(a, c, d, colour(mean));
        }
    }
}`,
  },
  {
    name: "torus",
    kind: "scene",
    glam: true,
    note: "Uses the glam crate from crates.io, pinned by a lock file.",
    cell: `use glam::{Quat, Vec3};
use std::f32::consts::TAU;

// A torus: rotate a small circle around the Y axis with glam quaternions.
fn scene(_ms: u32) {
    let (rings, sides) = (28_u32, 14_u32);
    let at = |i: u32, j: u32| {
        let a = (j % sides) as f32 / sides as f32 * TAU;
        let turn = Quat::from_rotation_y((i % rings) as f32 / rings as f32 * TAU);
        turn * Vec3::new(1.0 + 0.38 * a.cos(), 0.38 * a.sin(), 0.0)
    };
    for i in 0..rings {
        for j in 0..sides {
            let p = at(i, j);
            canvas3d::line(p.into(), at(i, j + 1).into(), [140, 160, 255]);
            canvas3d::line(p.into(), at(i + 1, j).into(), [140, 160, 255]);
        }
    }
}`,
  },
  {
    name: "library",
    kind: "scene",
    note: "The terrain's height function is a stored definition. Publish a new version, then pin the cell to either hash.",
    lib: {
      name: "terrain-surface",
      source: `pub fn height(x: f32, z: f32) -> f32 {
    let r = (x * x + z * z).sqrt();
    0.42 * (r * 5.0).cos() / (1.0 + 2.2 * r)
        + 0.12 * (x * 3.0).sin() * (z * 2.0).cos()
}
`,
    },
    cell: `// \`surface\` is a stored definition this cell depends on, pinned by hash.
fn scene(_ms: u32) {
    let n = 36;
    let colour = |h: f32| {
        let k = ((h + 0.25) / 0.6).clamp(0.0, 1.0);
        [(50.0 + 200.0 * k) as u8, (90.0 + 110.0 * k) as u8, (220.0 - 130.0 * k) as u8]
    };
    let at = |i: usize, j: usize| {
        let x = i as f32 / n as f32 * 2.4 - 1.2;
        let z = j as f32 / n as f32 * 2.4 - 1.2;
        [x, surface::height(x, z), z]
    };
    for i in 0..n {
        for j in 0..n {
            let (a, b, c, d) = (at(i, j), at(i + 1, j), at(i + 1, j + 1), at(i, j + 1));
            let mean = (a[1] + b[1] + c[1] + d[1]) / 4.0;
            canvas3d::tri(a, b, c, colour(mean));
            canvas3d::tri(a, c, d, colour(mean));
        }
    }
}`,
  },
  {
    name: "type error",
    kind: "value",
    note: "Compiler errors come back with line and column, shown as a red squiggle.",
    cell: `let x: u32 = "not a number";
x`,
  },
];
