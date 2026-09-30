// Edit one constant, time until the new result exists. usage: bun run editloop.ts <port>
import { readFileSync, writeFileSync } from "fs";
const port = process.argv[2];
const guestPath = "/Volumes/Projects/tmp/loom-rgb-spike/gen/guest.rs";
const corePath = "/Volumes/Projects/tmp/loom-rgb-spike/gen/gen_core.rs";
const guest0 = readFileSync(guestPath, "utf8");
const core0 = readFileSync(corePath, "utf8");
async function loomEval(src: string) {
  const t = performance.now();
  const r = await fetch(`http://127.0.0.1:${port}/v1/command`, { method: "POST", headers: { Authorization: "Bearer replbench", "Content-Type": "application/json" }, body: JSON.stringify({ command: "eval", args: { source: src, entry: "sweep", args: [0, 1] } }) });
  const j: any = await r.json();
  if (!j.ok) throw new Error(JSON.stringify(j).slice(0, 500));
  return { wall: performance.now() - t, hash: j.result.output[0] as string, t: j.result.timings_ms };
}
await loomEval(guest0); // warm the lineage
const loom: number[] = [], native: number[] = [], nativeInc: number[] = [];
for (let k = 1; k <= 8; k++) {
  const edit = (s: string) => s.replace("0.003 + 0.005 * next()", `0.003 + 0.00${k}0 * next()`);
  const g = edit(guest0);
  const l = await loomEval(g);
  loom.push(l.wall);
  writeFileSync(corePath, edit(core0));
  for (const [inc, arr] of [["0", native], ["1", nativeInc]] as const) {
    const t = performance.now();
    const b = Bun.spawnSync(["cargo", "build", "--release", "-j", "2", "--target-dir", inc === "0" ? "target-edit" : "target-edit-inc"], { cwd: "/Volumes/Projects/tmp/loom-rgb-spike/native", env: { ...process.env, CARGO_INCREMENTAL: inc } });
    const out = Bun.spawnSync(["./" + (inc === "0" ? "target-edit" : "target-edit-inc") + "/release/bracket-native", "1"], { cwd: "/Volumes/Projects/tmp/loom-rgb-spike/native" }).stdout.toString();
    arr.push(performance.now() - t);
    if (b.exitCode !== 0) throw new Error("native build failed");
    if (inc === "0") {
      const nh = out.match(/set0 fnv ([0-9a-f]+)/g)?.at(-1)?.split(" ")[2];
      console.log(`edit ${k}: loom wall ${l.wall.toFixed(0)} ms (compile ${l.t.compile}, run ${l.t.run}); native rebuild+run ${arr.at(-1)!.toFixed(0)} ms; result hashes ${l.hash === nh ? "equal" : "DIFFER " + l.hash + " vs " + nh}`);
    }
  }
}
writeFileSync(corePath, core0);
const med = (a: number[]) => [...a].sort((x, y) => x - y)[Math.floor(a.length / 2)];
console.log(`median: loom eval ${med(loom).toFixed(0)} ms | native cargo build --release -j2 + run ${med(native).toFixed(0)} ms (non-incremental) / ${med(nativeInc).toFixed(0)} ms (CARGO_INCREMENTAL=1)`);
