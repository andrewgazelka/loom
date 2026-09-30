export type Preset = { name: string; note: string; source: string; glam?: boolean; mesh?: boolean };

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

export const presets: Preset[] = [
  {
    name: "arithmetic",
    note: "a block is a cell: its last expression is the output",
    source: `let base: u64 = 6;
(1..=base).product::<u64>() + 7`,
  },
  {
    name: "primes",
    note: "real compute, compiled to wasm and run in a sandbox",
    source: `let limit = 200_000usize;
let mut sieve = vec![true; limit + 1];
sieve[0] = false;
sieve[1] = false;
let mut i = 2;
while i * i <= limit {
    if sieve[i] {
        let mut j = i * i;
        while j <= limit { sieve[j] = false; j += i; }
    }
    i += 1;
}
sieve.iter().filter(|&&p| p).count()`,
  },
  {
    name: "torus mesh",
    note: "uses glam from crates.io (pinned by lock); the page draws what Rust returns",
    glam: true,
    mesh: true,
    source: `use glam::{Quat, Vec3};
// A torus wireframe: rotate a small circle around the Y axis with glam quaternions.
let (major, minor, rings, sides) = (1.0_f32, 0.38_f32, 28_u32, 14_u32);
let mut points: Vec<f32> = Vec::new();
let mut edges: Vec<u32> = Vec::new();
for i in 0..rings {
    let around = Quat::from_rotation_y(i as f32 / rings as f32 * std::f32::consts::TAU);
    for j in 0..sides {
        let a = j as f32 / sides as f32 * std::f32::consts::TAU;
        let p = around * Vec3::new(major + minor * a.cos(), minor * a.sin(), 0.0);
        points.extend_from_slice(&[p.x, p.y, p.z]);
        let here = i * sides + j;
        edges.extend_from_slice(&[here, i * sides + (j + 1) % sides]);
        edges.extend_from_slice(&[here, ((i + 1) % rings) * sides + j]);
    }
}
(points, edges)`,
  },
  {
    name: "type error",
    note: "rustc diagnostics come back with line numbers",
    source: `let x: u32 = "not a number";
x`,
  },
];
