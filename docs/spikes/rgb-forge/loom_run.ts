// Run the bracket guest in a Loom daemon and compare with the native hashes.
// usage: bun run loom_run.ts <port> [optimize=true|false] [reps=9]
import { readFileSync } from "fs";
const port = process.argv[2];
const optimize = (process.argv[3] ?? "true") === "true";
const reps = Number(process.argv[4] ?? 9);
const source = readFileSync("/Volumes/Projects/tmp/loom-rgb-spike/gen/guest.rs", "utf8");

async function ev(entry: string, args: any[], src = source, extra: any = {}) {
  const t = performance.now();
  const r = await fetch(`http://127.0.0.1:${port}/v1/command`, {
    method: "POST",
    headers: { Authorization: "Bearer replbench", "Content-Type": "application/json" },
    body: JSON.stringify({ command: "eval", args: { source: src, entry, args, optimize, ...extra } }),
  });
  const j: any = await r.json();
  return { wall: performance.now() - t, j };
}

const FNV = (bytes: number[]) => {
  let h = 0xcbf29ce484222325n;
  for (const b of bytes) { h ^= BigInt(b); h = (h * 0x100000001b3n) & 0xffffffffffffffffn; }
  return h;
};

const first = await ev("bytes", [0]);
if (!first.j.ok) { console.log("FAILED", JSON.stringify(first.j).slice(0, 1500)); process.exit(1); }
const b0: number[] = first.j.result.output;
console.log(`optimize=${optimize} compile ${first.j.result.timings_ms.compile} ms (cold cell), set0 bytes ${b0.length} fnv ${FNV(b0).toString(16).padStart(16, "0")}`);

const runs: number[] = [];
let hashes: string[] = [];
for (let i = 0; i < reps; i++) {
  const r = await ev("sweep", [0, 1000]);
  if (!r.j.ok) { console.log("FAILED", JSON.stringify(r.j).slice(0, 800)); process.exit(1); }
  runs.push(r.j.result.timings_ms.run);
  hashes = r.j.result.output;
}
runs.sort((a, b) => a - b);
let all = 0xcbf29ce484222325n;
for (const h of hashes) {
  const le: number[] = []; let a = all; for (let k = 0; k < 8; k++) { le.push(Number(a & 0xffn)); a >>= 8n; }
  let v = BigInt("0x" + h); for (let k = 0; k < 8; k++) { le.push(Number(v & 0xffn)); v >>= 8n; }
  all = FNV(le);
}
const refused = hashes.filter((h) => h === "0".repeat(16)).length;
console.log(`sweep 1000 sets: run best ${runs[0]} ms, median ${runs[Math.floor(runs.length / 2)]} ms (includes instantiate + result encode), refused ${refused}, combined fnv ${all.toString(16).padStart(16, "0")}`);
for (const i of [0, 1, 500, 999]) console.log(`set${i} fnv ${hashes[i]}`);
