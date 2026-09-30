import { readFileSync } from "fs";
const aos = readFileSync("/Volumes/Projects/tmp/loom-rgb-spike/gen/guest.rs", "utf8");
const soa = readFileSync("/Volumes/Projects/tmp/loom-rgb-spike/gen/guest_soa.rs", "utf8");
async function run(source: string) {
  const r = await fetch(`http://127.0.0.1:8826/v1/command`, { method: "POST", headers: { Authorization: "Bearer replbench", "Content-Type": "application/json" }, body: JSON.stringify({ command: "eval", args: { source, entry: "sweep", args: [0, 1000], optimize: true } }) });
  const j: any = await r.json(); if (!j.ok) throw new Error(JSON.stringify(j).slice(0, 600)); return j.result;
}
const bin = (name: string) => Number(Bun.spawnSync([`/Volumes/Projects/tmp/loom-rgb-spike/native/target-o2/release/${name}`, "1000"]).stdout.toString().match(/best ([0-9.]+) ms total/)![1]);
const a0 = await run(aos), s0 = await run(soa);
console.log("hashes identical AoS vs SoA (wasm, 1000 sets):", JSON.stringify(a0.output) === JSON.stringify(s0.output));
const natA = Bun.spawnSync(["/Volumes/Projects/tmp/loom-rgb-spike/native/target-o2/release/bracket-native", "1000"]).stdout.toString().match(/combined fnv (\w+)/)![1];
const natS = Bun.spawnSync(["/Volumes/Projects/tmp/loom-rgb-spike/native/target-o2/release/soa", "1000"]).stdout.toString().match(/combined fnv (\w+)/)![1];
console.log("native combined fnv AoS", natA, "SoA", natS);
const t: Record<string, number[]> = { "native AoS": [], "native SoA": [], "wasm AoS": [], "wasm SoA": [] };
for (let i = 0; i < 9; i++) {
  t["native AoS"].push(bin("bracket-native")); t["native SoA"].push(bin("soa"));
  t["wasm AoS"].push((await run(aos)).timings_ms.run); t["wasm SoA"].push((await run(soa)).timings_ms.run);
}
const best = (a: number[]) => Math.min(...a);
for (const k of Object.keys(t)) console.log(k.padEnd(11), `best ${best(t[k]).toFixed(1)} ms per 1000 calls`);
console.log(`SoA speedup: native ${(best(t["native AoS"]) / best(t["native SoA"])).toFixed(2)}x, wasm ${(best(t["wasm AoS"]) / best(t["wasm SoA"])).toFixed(2)}x; wasm/native AoS ${(best(t["wasm AoS"]) / best(t["native AoS"])).toFixed(2)}x SoA ${(best(t["wasm SoA"]) / best(t["native SoA"])).toFixed(2)}x`);
