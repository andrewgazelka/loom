import { readFileSync } from "fs";
const source = readFileSync("/Volumes/Projects/tmp/loom-rgb-spike/gen/guest.rs", "utf8");
async function run(port: string, entry: string, args: any[]) {
  const r = await fetch(`http://127.0.0.1:${port}/v1/command`, { method: "POST", headers: { Authorization: "Bearer replbench", "Content-Type": "application/json" }, body: JSON.stringify({ command: "eval", args: { source, entry, args, optimize: true } }) });
  const j: any = await r.json(); if (!j.ok) throw new Error(JSON.stringify(j).slice(0, 400)); return j.result;
}
const N2 = "/Volumes/Projects/tmp/loom-rgb-spike/native/target-o2/release/bracket-native";
const native = () => Number(Bun.spawnSync([N2, "1000"]).stdout.toString().match(/best ([0-9.]+) ms total/)![1]);
const outs: Record<string, string[]> = {};
for (const port of ["8826", "8827"]) { const r = await run(port, "sweep", [0, 1000]); outs[port] = r.output; }
console.log("hashes identical with and without NaN canonicalization:", JSON.stringify(outs["8826"]) === JSON.stringify(outs["8827"]));
const t: Record<string, number[]> = { native: [], off: [], on: [] };
for (let i = 0; i < 9; i++) {
  t.native.push(native());
  t.off.push((await run("8826", "sweep", [0, 1000])).timings_ms.run);
  t.on.push((await run("8827", "sweep", [0, 1000])).timings_ms.run);
}
const best = (a: number[]) => Math.min(...a), med = (a: number[]) => [...a].sort((x, y) => x - y)[4];
for (const k of Object.keys(t)) console.log(k.padEnd(7), `best ${best(t[k]).toFixed(1)} ms  median ${med(t[k]).toFixed(1)} ms per 1000 calls`);
console.log(`wasm/native ${ (best(t.off)/best(t.native)).toFixed(2) }x, wasm+canon/native ${(best(t.on)/best(t.native)).toFixed(2)}x, canon cost ${(best(t.on)/best(t.off)).toFixed(2)}x`);
