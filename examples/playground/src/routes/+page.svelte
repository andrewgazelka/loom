<script lang="ts">
  import { onMount } from "svelte";
  import "@fontsource/inter/400.css";
  import "@fontsource/inter/600.css";
  import "@fontsource/inter/700.css";
  import { presets, prelude as basePrelude, manifest, lock, type Preset } from "$lib/presets";
  import Editor from "$lib/Editor.svelte";
  import Stage from "$lib/Stage.svelte";

  let current: Preset = $state(presets[0]);
  let cell = $state(presets[0].cell);
  let prelude = $state(basePrelude);
  let tab: "cell" | "canvas" = $state("cell");
  let busy = $state(false);
  let reply: any = $state.raw(null);
  let scene: any[] = $state.raw([]); // raw: a deep proxy over thousands of commands makes every frame crawl
  let frameMs = $state(0);
  let ticket = 0;
  let timer: ReturnType<typeof setTimeout>;

  const cellLines = $derived(cell.split("\n").length);
  const diagnostics = $derived(
    (reply && !reply.ok ? (reply.diagnostics ?? []) : []).filter((d: any) => !d.message.startsWith("aborting due to")),
  );
  // The prelude is appended after the cell (a blank line between), so the cell's line numbers are the user's own.
  const problems = $derived(
    tab === "cell"
      ? diagnostics.filter((d: any) => d.line <= cellLines)
      : diagnostics.filter((d: any) => d.line > cellLines + 1).map((d: any) => ({ ...d, line: d.line - cellLines - 1 })),
  );
  const elsewhere = $derived(diagnostics.length - problems.length);
  const timings = $derived(reply?.result?.timings_ms);
  const counts = $derived(
    Object.entries(scene.reduce((m: Record<string, number>, c: any) => ((m[c.op] = (m[c.op] ?? 0) + 1), m), {})) as [string, number][],
  );
  const active = {
    get value() { return tab === "cell" ? cell : prelude; },
    set value(v: string) { if (tab === "cell") cell = v; else prelude = v; },
  };

  async function post(path: string, body: unknown) {
    const res = await fetch(path, { method: "POST", body: JSON.stringify(body) });
    return res.json();
  }

  async function run() {
    clearTimeout(timer);
    const mine = ++ticket;
    busy = true;
    const body = await post("/api/eval", {
      source: `${cell}\n\n${prelude}`,
      entry: "frame",
      args: [0],
      ...(current.glam ? { manifest, lock } : {}),
    });
    if (mine !== ticket) return; // a newer edit replaced this run
    reply = body;
    busy = false;
    if (!body.ok) return;
    scene = body.result.output;
    if (current.animate) animate(mine, body.result.hash);
  }

  // Animated cells: built once, then the page asks Rust for frame(t) in a loop.
  async function animate(mine: number, hash: string) {
    const start = performance.now();
    while (mine === ticket) {
      const t0 = performance.now();
      const r = await post("/api/run", { target: hash, args: [Math.round(t0 - start)] });
      if (mine !== ticket) return;
      if (!r.ok) return;
      scene = r.result.output;
      frameMs = Math.round(performance.now() - t0);
      await new Promise((ok) => setTimeout(ok, Math.max(0, 16 - (performance.now() - t0))));
    }
  }

  function pick(p: Preset) {
    current = p;
    cell = p.cell;
    tab = "cell";
    run();
  }

  // Live: a pause in typing rebuilds the cell (a warm edit costs about 80 ms).
  let first = true;
  $effect(() => {
    cell; prelude;
    if (first) { first = false; return; }
    clearTimeout(timer);
    timer = setTimeout(run, 450);
  });
  onMount(() => { run(); return () => { ticket++; clearTimeout(timer); }; });
</script>

<svelte:head><title>Loom Playground</title></svelte:head>

<main>
  <header>
    <h1>Loom</h1>
    <p>Rust in, pixels out. Drawing is an algebraic effect: the cell performs <code>canvas2d</code> and <code>canvas3d</code> effects, a handler collects them, the page paints.</p>
  </header>

  <nav>
    {#each presets as p}
      <button class:on={p === current} onclick={() => pick(p)}>{p.name}</button>
    {/each}
    <span class="note">{current.note}</span>
  </nav>

  <div class="grid">
    <section class="card">
      <div class="bar">
        <button class="tab" class:on={tab === "cell"} onclick={() => (tab = "cell")}>cell.rs</button>
        <button class="tab" class:on={tab === "canvas"} onclick={() => (tab = "canvas")}>canvas.rs</button>
        <span class="spacer"></span>
        {#if elsewhere > 0}<span class="warn">{elsewhere} more in {tab === "cell" ? "canvas.rs" : "cell.rs"}</span>{/if}
        <kbd>⌘↵</kbd>
        <button class="run" onclick={run} disabled={busy}>{busy ? "Building" : "Run"}</button>
      </div>
      <Editor bind:value={active.value} {run} {problems} />
    </section>

    <section class="card stage">
      <div class="bar">
        <span class="file">stage</span>
        <span class="spacer"></span>
        {#if reply?.ok}
          <span class="chip"><b>{reply.wall_ms}</b> ms build + run</span>
          {#if current.animate && frameMs}<span class="chip"><b>{frameMs}</b> ms / frame</span>{/if}
        {/if}
      </div>
      {#if reply && !reply.ok}
        <ul class="errors">
          {#each diagnostics as d}<li><span class="where">{d.line}:{d.col}</span>{d.message}</li>{/each}
          {#if !diagnostics.length}<li>{reply.result?.error ?? JSON.stringify(reply)}</li>{/if}
        </ul>
      {:else}
        <Stage {scene} />
        <p class="effects">
          {#each counts as [op, n]}<span><b>{op}</b> × {n}</span>{/each}
          {#if !counts.length}running…{/if}
        </p>
      {/if}
    </section>
  </div>
</main>

<style>
  :global(:root) { --bg: #f6f5f2; --ink: #1d1d1b; --dim: #7b7972; --accent: #6b7cf0; color-scheme: light dark; }
  @media (prefers-color-scheme: dark) { :global(:root) { --bg: #0d0e13; --ink: #e8e9f0; --dim: #7d819a; --accent: #8d9bff; } }
  :global(body) { margin: 0; background: var(--bg); color: var(--ink); font: 16px/1.5 "Inter", system-ui, sans-serif; -webkit-font-smoothing: antialiased; }
  main { max-width: 1280px; margin: 0 auto; padding: 48px 20px 80px; }
  h1 { margin: 0; font-size: 34px; font-weight: 700; letter-spacing: -0.03em; }
  header p { margin: 4px 0 0; color: var(--dim); max-width: 760px; }
  code { font-family: "JetBrains Mono", ui-monospace, monospace; font-size: 0.9em; color: var(--ink); }
  nav { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; margin: 26px 0 14px; }
  nav button { border: 0; background: transparent; color: var(--dim); padding: 6px 12px; border-radius: 999px; cursor: pointer; font: inherit; font-size: 14px; }
  nav button:hover { color: var(--ink); }
  nav button.on { background: var(--ink); color: var(--bg); }
  .note { margin-left: 10px; color: var(--dim); font-size: 13px; }
  .grid { display: grid; grid-template-columns: minmax(0, 1.1fr) minmax(0, 1fr); gap: 16px; align-items: start; }
  @media (max-width: 1000px) { .grid { grid-template-columns: minmax(0, 1fr); } .stage { order: -1; } }
  .card { background: #14151c; color: #c8d0f0; border-radius: 14px; overflow: hidden; box-shadow: 0 0 0 1px rgba(255,255,255,0.05); }
  .bar { display: flex; align-items: center; gap: 8px; padding: 8px 14px; border-bottom: 1px solid rgba(255,255,255,0.06); font-size: 12.5px; min-height: 30px; }
  .file { color: #7a86b8; font-family: "JetBrains Mono", ui-monospace, monospace; }
  .tab { border: 0; background: transparent; color: #545b7f; font: 12.5px "JetBrains Mono", ui-monospace, monospace; padding: 4px 8px; border-radius: 6px; cursor: pointer; }
  .tab.on { color: #c8d0f0; background: rgba(255,255,255,0.06); }
  .spacer { flex: 1; }
  .warn { color: #ff9e64; }
  kbd { color: #545b7f; font: 12px "JetBrains Mono", monospace; }
  .run { border: 0; background: #7aa2f7; color: #0d0e13; padding: 5px 16px; border-radius: 8px; font: 600 13px Inter, system-ui, sans-serif; cursor: pointer; }
  .run:disabled { opacity: .55; cursor: progress; }
  .chip { color: #7a86b8; background: rgba(255,255,255,0.05); padding: 2px 9px; border-radius: 999px; font-variant-numeric: tabular-nums; }
  .chip b { color: #c8d0f0; font-weight: 600; }
  .effects { display: flex; gap: 14px; flex-wrap: wrap; margin: 0; padding: 10px 16px 14px; color: #545b7f; font: 12px "JetBrains Mono", ui-monospace, monospace; }
  .effects b { color: #7a86b8; font-weight: 500; }
  .errors { list-style: none; margin: 0; padding: 16px; font: 13.5px/1.65 "JetBrains Mono", ui-monospace, monospace; color: #f7768e; min-height: 200px; }
  .errors li { margin-bottom: 6px; }
  .where { color: #545b7f; margin-right: 12px; }
</style>
