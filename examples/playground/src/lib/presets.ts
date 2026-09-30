export type Preset = { name: string; note: string; cell: string; glam?: boolean; animate?: boolean };

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

// The drawing effects and the handler that interprets them. Shown in the canvas.rs tab and appended to every cell.
export const prelude = `// Drawing is an effect. canvas2d and canvas3d only *perform* labelled effects;
// \`collect\` is the handler that decides what they mean: here, append to a scene.
use loom::{Continuation, Effect, Reply, Value, serde_json::json};

type Rgb = [u8; 3];
type Scene = Vec<Value>;

mod canvas2d {
    use super::*;
    pub fn line(a: [f32; 2], b: [f32; 2], color: Rgb) {
        let _ = loom::perform::<()>("canvas2d.line", json!({ "a": a, "b": b, "color": color }));
    }
    pub fn circle(at: [f32; 2], radius: f32, color: Rgb) {
        let _ = loom::perform::<()>("canvas2d.circle", json!({ "at": at, "r": radius, "color": color }));
    }
}

mod canvas3d {
    use super::*;
    pub fn line(a: [f32; 3], b: [f32; 3], color: Rgb) {
        let _ = loom::perform::<()>("canvas3d.line", json!({ "a": a, "b": b, "color": color }));
    }
    pub fn tri(a: [f32; 3], b: [f32; 3], c: [f32; 3], color: Rgb) {
        let _ = loom::perform::<()>("canvas3d.tri", json!({ "a": a, "b": b, "c": c, "color": color }));
    }
}

fn collect(body: impl FnOnce()) -> Scene {
    let mut scene = Scene::new();
    let labels = ["canvas2d.line", "canvas2d.circle", "canvas3d.line", "canvas3d.tri"];
    loom::handle(labels, |e: Effect, _k: Continuation| {
        scene.push(json!({ "op": e.name, "args": e.args }));
        Reply::Resume(Value::Null)
    }, body).expect("handler failed");
    scene
}`;

export const presets: Preset[] = [
  {
    name: "spirograph",
    note: "canvas2d effects, animated: the page calls the Rust frame function every tick.",
    animate: true,
    cell: `use std::f32::consts::TAU;

// A hypotrochoid: a circle rolling inside a circle, with a pen on the
// rolling one. The argument is the page clock in milliseconds.
pub fn frame(ms: u32) -> Scene {
    let t = ms as f32 / 1000.0;
    collect(|| {
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
    })
}`,
  },
  {
    name: "terrain",
    note: "canvas3d effects: drag to orbit, scroll to zoom. Rust builds the mesh once.",
    cell: `// A height field over a grid, two triangles per cell, coloured by height.
pub fn frame(_ms: u32) -> Scene {
    collect(|| {
        let n = 36;
        let height = |x: f32, z: f32| {
            let r = (x * x + z * z).sqrt();
            0.42 * (r * 5.0).cos() / (1.0 + 2.2 * r) + 0.12 * (x * 3.0).sin() * (z * 2.0).cos()
        };
        let colour = |h: f32| {
            let k = ((h + 0.25) / 0.6).clamp(0.0, 1.0);
            [(50.0 + 200.0 * k) as u8, (90.0 + 110.0 * k) as u8, (220.0 - 130.0 * k) as u8]
        };
        let at = |i: usize, j: usize| {
            let (x, z) = (i as f32 / n as f32 * 2.4 - 1.2, j as f32 / n as f32 * 2.4 - 1.2);
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
    })
}`,
  },
  {
    name: "torus",
    note: "canvas3d lines with glam quaternions (crates.io, pinned by a lock).",
    glam: true,
    cell: `use glam::{Quat, Vec3};
use std::f32::consts::TAU;

pub fn frame(_ms: u32) -> Scene {
    collect(|| {
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
    })
}`,
  },
  {
    name: "type error",
    note: "rustc diagnostics come back with line and column.",
    cell: `pub fn frame(_ms: u32) -> Scene {
    let x: u32 = "not a number";
    collect(|| canvas2d::circle([0.0, 0.0], x as f32, [255, 0, 0]))
}`,
  },
];
