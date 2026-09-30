import { readFileSync } from "fs";
const port = process.argv[2]; const optimize = (process.argv[3] ?? "true") === "true";
const source = readFileSync("/Volumes/Projects/tmp/loom-rgb-spike/gen/guest.rs", "utf8");
async function loomRun() {
  const r = await fetch(`http://127.0.0.1:${port}/v1/command`, { method: "POST", headers: { Authorization: "Bearer replbench", "Content-Type": "application/json" }, body: JSON.stringify({ command: "eval", args: { source, entry: "sweep", args: [0, 1000], optimize } }) });
  const j: any = await r.json(); return j.result.timings_ms.run as number;
}
function native(bin: string) {
  const o = Bun.spawnSync([bin, "1000"]).stdout.toString();
  return Number(o.match(/best ([0-9.]+) ms total/)![1]);
}
await loomRun(); // warm
const N3 = "/Volumes/Projects/tmp/loom-rgb-spike/native/target/release/bracket-native";
const N2 = "/Volumes/Projects/tmp/loom-rgb-spike/native/target-o2/release/bracket-native";
const rows: Record<string, number[]> = { "native opt3": [], "native opt2": [], "loom wasm": [] };
for (let i = 0; i < 9; i++) { rows["native opt3"].push(native(N3)); rows["loom wasm"].push(await loomRun()); rows["native opt2"].push(native(N2)); }
const med = (a: number[]) => [...a].sort((x, y) => x - y)[Math.floor(a.length / 2)];
const min = (a: number[]) => Math.min(...a);
for (const [k, v] of Object.entries(rows)) console.log(k.padEnd(12), `best ${min(v).toFixed(1)} ms  median ${med(v).toFixed(1)} ms per 1000 calls`);
console.log(`ratio wasm/native-opt2 (best) ${(min(rows["loom wasm"]) / min(rows["native opt2"])).toFixed(2)}x, wasm/native-opt3 ${(min(rows["loom wasm"]) / min(rows["native opt3"])).toFixed(2)}x`);
