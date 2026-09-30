<script lang="ts">
  import { presets, manifest, lock, type Preset } from "$lib/presets";
  import Editor from "$lib/Editor.svelte";

  let current: Preset = $state(presets[2]);
  let source = $state(presets[2].source);
  let busy = $state(false);
  let reply: any = $state(null);
  let history: { name: string; ms: number }[] = $state([]);
  let canvas: HTMLCanvasElement | undefined = $state();
  let mesh: { points: number[]; edges: number[] } | null = $state(null);

  function pick(p: Preset) {
    current = p;
    source = p.source;
  }

  async function run() {
    busy = true;
    const res = await fetch("/api/eval", {
      method: "POST",
      body: JSON.stringify({ source, ...(current.glam ? { manifest, lock } : {}) }),
    });
    reply = await res.json();
    busy = false;
    const out = reply?.result?.output;
    mesh = current.mesh && Array.isArray(out) ? { points: out[0], edges: out[1] } : null;
    history = [{ name: current.name, ms: reply.wall_ms }, ...history].slice(0, 8);
  }

  $effect(() => {
    if (!canvas || !mesh) return;
    const ctx = canvas.getContext("2d")!;
    const m = mesh;
    let raf = 0;
    const draw = (t: number) => {
      const w = canvas!.width, h = canvas!.height;
      ctx.clearRect(0, 0, w, h);
      ctx.strokeStyle = getComputedStyle(canvas!).getPropertyValue("--ink");
      ctx.lineWidth = 1;
      const a = t / 2600, b = 0.6;
      const pr: [number, number][] = [];
      for (let i = 0; i < m.points.length; i += 3) {
        const [x, y, z] = [m.points[i], m.points[i + 1], m.points[i + 2]];
        const x1 = x * Math.cos(a) + z * Math.sin(a), z1 = -x * Math.sin(a) + z * Math.cos(a);
        const y2 = y * Math.cos(b) - z1 * Math.sin(b);
        pr.push([w / 2 + x1 * w * 0.3, h / 2 + y2 * w * 0.3]);
      }
      ctx.beginPath();
      for (let i = 0; i < m.edges.length; i += 2) {
        const p = pr[m.edges[i]], q = pr[m.edges[i + 1]];
        ctx.moveTo(p[0], p[1]);
        ctx.lineTo(q[0], q[1]);
      }
      ctx.stroke();
      raf = requestAnimationFrame(draw);
    };
    raf = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(raf);
  });
</script>

<main>
  <h1>Loom playground</h1>
  <p class="sub">Rust in, result out. Each run compiles your cell to WebAssembly and runs it in a sandbox.</p>

  <nav>
    {#each presets as p}
      <button class:on={p === current} onclick={() => pick(p)}>{p.name}</button>
    {/each}
  </nav>
  <p class="note">{current.note}</p>

  <Editor bind:value={source} {run} problems={reply && !reply.ok ? (reply.diagnostics ?? []) : []} />
  <div class="bar">
    <button class="run" onclick={run} disabled={busy}>{busy ? "running…" : "Run"}</button>
    {#if reply}
      <span class="stat">
        {reply.wall_ms} ms wall
        {#if reply.result?.timings_ms}· compile {reply.result.timings_ms.compile} ms · run {reply.result.runtime_ms?.run_ms?.toFixed(2)} ms{/if}
      </span>
    {/if}
  </div>

  {#if reply}
    {#if reply.ok}
      {#if mesh}
        <canvas bind:this={canvas} width="640" height="360"></canvas>
        <p class="note">{mesh.points.length / 3} points, {mesh.edges.length / 2} edges, computed in the guest</p>
      {:else}
        <pre class="out">{JSON.stringify(reply.result.output, null, 2)}</pre>
      {/if}
    {:else}
      <pre class="out err">{#each reply.diagnostics ?? [] as d}{d.line ? `line ${d.line}: ` : ""}{d.message}
{/each}{#if !(reply.diagnostics ?? []).length}{JSON.stringify(reply, null, 2)}{/if}</pre>
    {/if}
  {/if}

  {#if history.length}
    <p class="note">recent: {history.map((h) => `${h.name} ${h.ms} ms`).join(" · ")}</p>
  {/if}
</main>

<style>
  :global(:root) {
    --bg: #fbfbfa; --ink: #1f1f1d; --dim: #77756f; --panel: #f1f0ed; --accent: #d9622b; --err: #b3261e;
    --s-keyword: #b0357a; --s-type: #1a7f7a; --s-function: #2c5fb3; --s-macro: #a4581c; --s-string: #3b7d22; --s-number: #b5541c; --s-punct: #77756f; --s-prop: #1f1f1d;
    color-scheme: light dark;
  }
  @media (prefers-color-scheme: dark) {
    :global(:root) { --bg: #191918; --ink: #ecebe7; --dim: #9b9992; --panel: #242422; --accent: #ef8a57; --err: #f2867e;
      --s-keyword: #ff8ac0; --s-type: #6fd6cf; --s-function: #8db4ff; --s-macro: #f0a868; --s-string: #a5d98a; --s-number: #f2a979; --s-punct: #9b9992; --s-prop: #ecebe7; }
  }
  :global(body) { margin: 0; background: var(--bg); color: var(--ink); font: 16px/1.5 system-ui, sans-serif; }
  main { max-width: 720px; margin: 0 auto; padding: 48px 16px 80px; }
  h1 { font-size: 28px; margin: 0; letter-spacing: -0.02em; }
  .sub, .note { color: var(--dim); font-size: 14px; margin: 6px 0 16px; }
  nav { display: flex; gap: 8px; flex-wrap: wrap; margin-top: 24px; }
  nav button { border: 0; background: var(--panel); color: var(--ink); padding: 6px 12px; border-radius: 8px; cursor: pointer; font: inherit; font-size: 14px; }
  nav button.on { background: var(--ink); color: var(--bg); }
  .bar { display: flex; align-items: center; gap: 14px; margin: 12px 0; }
  .run { border: 0; background: var(--accent); color: #fff; padding: 8px 20px; border-radius: 8px; font: inherit; font-weight: 600; cursor: pointer; }
  .run:disabled { opacity: 0.6; cursor: progress; }
  .stat { color: var(--dim); font-size: 13px; font-variant-numeric: tabular-nums; }
  .out { background: var(--panel); border-radius: 10px; padding: 14px; font: 13px/1.5 ui-monospace, Menlo, monospace; overflow: auto; white-space: pre-wrap; }
  .err { color: var(--err); }
  canvas { width: 100%; height: auto; background: var(--panel); border-radius: 10px; }
</style>
