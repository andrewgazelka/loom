<script lang="ts">
  import { onMount } from "svelte";
  import "@fontsource/inter/400.css";
  import "@fontsource/inter/600.css";
  import "@fontsource/inter/700.css";
  import { presets, prelude as basePrelude, manifest, lock, type Preset } from "$lib/presets";
  import Editor from "$lib/Editor.svelte";
  import Stage from "$lib/Stage.svelte";

  let current: Preset = $state.raw(presets[0]); // raw: `p === current` must compare the preset objects themselves
  let cell = $state(presets[0].cell);
  let prelude = $state(basePrelude);
  let tab: "cell" | "canvas" | "lib" = $state("cell");
  let busy = $state(false);
  let reply: any = $state.raw(null);
  let scene: any[] = $state.raw([]); // raw: a deep proxy over thousands of commands makes every frame crawl
  let frameMs = $state(0);
  let drawMs = $state(0);
  // Stored definition the cell depends on (Unison-style): name, hash chain, the hash the cell pins.
  type Rev = { hash: string; timestamp: number; changed: string[] };
  let lib: { revs: Rev[]; pinned: string; source: string; error: string; busy: boolean } | null = $state(null);
  let seen: string[] = $state([]); // cell hashes in this session, newest first
  let ticket = 0;
  let timer: ReturnType<typeof setTimeout>;

  const cellLines = $derived(cell.split("\n").length);
  const diagnostics = $derived(
    (reply && !reply.ok ? (reply.diagnostics ?? []) : []).filter((d: any) => !d.message.startsWith("aborting due to")),
  );
  // The prelude is appended after the cell (a blank line between), so the cell's line numbers are the user's own.
  const problems = $derived(
    tab === "cell" || !isScene
      ? diagnostics.filter((d: any) => d.line <= cellLines)
      : diagnostics.filter((d: any) => d.line > cellLines + 1).map((d: any) => ({ ...d, line: d.line - cellLines - 1 })),
  );
  const elsewhere = $derived(diagnostics.length - problems.length);
  const timings = $derived(reply?.result?.timings_ms);
  const KINDS = ["canvas2d.line", "canvas2d.circle", "canvas3d.line", "canvas3d.tri"];
  const isScene = $derived(current.kind === "scene");
  const counts = $derived(
    Object.entries(scene.reduce((m: Record<string, number>, c: any) => ((m[KINDS[c[0]]] = (m[KINDS[c[0]]] ?? 0) + 1), m), {})) as [string, number][],
  );
  let value: unknown = $state.raw(null); // result of a "value" cell
  const active = {
    get value() { return tab === "cell" ? cell : tab === "canvas" ? prelude : (lib?.source ?? ""); },
    set value(v: string) { if (tab === "cell") cell = v; else if (tab === "canvas") prelude = v; else if (lib) lib.source = v; },
  };
  const short = (h: string) => h.slice(0, 8);
  // Short values on one line, long ones indented.
  const show = (v: unknown) => { const one = JSON.stringify(v); return one.length <= 90 ? one : JSON.stringify(v, null, 2); };
  const head = $derived(lib?.revs.at(-1)?.hash);
  const cellHash = $derived(reply?.ok ? (reply.result.hash as string) : null);

  async function post(path: string, body: unknown) {
    const res = await fetch(path, { method: "POST", body: JSON.stringify(body) });
    return res.json();
  }

  async function run() {
    clearTimeout(timer);
    const mine = ++ticket;
    busy = true;
    const body = await post("/api/eval", {
      source: isScene ? `${cell}\n\n${prelude}` : cell,
      ...(isScene ? { entry: "frame", args: [0] } : {}),
      ...(current.glam ? { manifest, lock } : {}),
      ...(current.lib && lib ? { deps: { surface: lib.pinned } } : {}),
    });
    if (mine !== ticket) return; // a newer edit replaced this run
    reply = body;
    busy = false;
    if (!body.ok) return;
    if (isScene) scene = body.result.output; else value = body.result.output;
    const h = body.result.hash as string;
    if (seen[0] !== h) seen = [h, ...seen].slice(0, 6);
    if (current.animate) animate(mine, body.result.hash);
  }

  // Animated cells: built once, then the page asks Rust for frame(t) in a loop.
  async function animate(mine: number, hash: string) {
    const start = performance.now();
    while (mine === ticket) {
      // A background tab paints nothing: stop asking Rust for frames until it is visible again.
      if (document.hidden) { await new Promise((ok) => setTimeout(ok, 300)); continue; }
      const t0 = performance.now();
      const r = await post("/api/run", { target: hash, args: [Math.round(t0 - start)] });
      if (mine !== ticket) return;
      if (!r.ok) return;
      scene = r.result.output;
      frameMs = Math.round(performance.now() - t0);
      await new Promise((ok) => setTimeout(ok, Math.max(0, 16 - (performance.now() - t0))));
    }
  }

  const command = (c: string, args: unknown) => post("/api/cmd", { command: c, args });

  // Load (or create) the stored definition and pin the cell to its newest hash.
  async function loadLib(def: { name: string; source: string }) {
    let h = await command("history", { name: def.name });
    if (!h.ok) {
      const added = await command("add", { name: def.name, lang: "rust", source: def.source });
      if (!added.ok) { lib = { revs: [], pinned: "", source: def.source, error: added.result?.error ?? "add failed", busy: false }; return; }
      h = await command("history", { name: def.name });
    }
    const revs: Rev[] = h.result.map((r: any) => ({
      hash: r.hash,
      timestamp: r.timestamp,
      changed: (r.changes?.changed ?? []).map((c: any) => c.name),
    }));
    const latest = revs.at(-1)!.hash;
    const viewed = await command("view", { target: latest });
    lib = { revs, pinned: latest, source: viewed.ok ? (viewed.result.formatted_source ?? viewed.result.source) : def.source, error: "", busy: false };
  }

  // Publish the edited source as a new revision of the name. Cells pinned to the old hash keep running the old code.
  async function publish() {
    if (!lib || !current.lib) return;
    lib.busy = true; lib.error = "";
    const u = await command("update", { name: current.lib.name, source: lib.source });
    const problems = (u.result?.update?.diagnostics ?? []).flatMap((d: any) => d.diagnostics ?? []);
    if (!u.ok || (u.result?.update && !u.result.update.changes?.length && problems.length)) {
      lib.error = u.ok ? problems.map((d: any) => d.message).join("\n") : (u.result?.error ?? "update failed");
      lib.busy = false;
      return;
    }
    const h = await command("history", { name: current.lib.name });
    if (h.ok) lib.revs = h.result.map((r: any) => ({ hash: r.hash, timestamp: r.timestamp, changed: (r.changes?.changed ?? []).map((c: any) => c.name) }));
    lib.busy = false;
  }

  function pin(hash: string) {
    if (!lib || lib.pinned === hash) return;
    lib.pinned = hash;
    run();
  }

  async function pick(p: Preset) {
    current = p;
    cell = p.cell;
    tab = "cell";
    lib = null;
    seen = [];
    scene = [];
    value = null;
    reply = null;
    if (p.lib) await loadLib(p.lib);
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
    <p class="lede">Write Rust in the browser. Loom compiles it to WebAssembly and runs it in a sandbox in milliseconds. Every example below is a real Rust program running on a Loom server.</p>
    <div class="ideas">
      <div><h3>Sandboxed</h3><p>Each cell runs as WebAssembly with its own memory. It can only do what the host allows.</p></div>
      <div><h3>Named by content</h3><p>A function is identified by the hash of what it means. Rename a variable: same hash. Change behaviour: new hash. A dependency pins a hash, so it never changes under you.</p></div>
      <div><h3>Effects</h3><p>Code does not draw, read files or sleep directly. It asks ("performs an effect") and a handler decides what that means. Here, drawing is an effect.</p></div>
    </div>
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
        {#if isScene}<button class="tab" class:on={tab === "canvas"} onclick={() => (tab = "canvas")} title="How drawing works: the effects and the handler that collects them">how drawing works</button>{/if}
        {#if lib}<button class="tab" class:on={tab === "lib"} onclick={() => (tab = "lib")}>surface.rs</button>{/if}
        <span class="spacer"></span>
        {#if elsewhere > 0}<span class="warn">{elsewhere} more in {tab === "cell" ? "canvas.rs" : "cell.rs"}</span>{/if}
        <kbd>⌘↵</kbd>
        <button class="run" onclick={run} disabled={busy}>{busy ? "Building" : "Run"}</button>
      </div>
      <Editor bind:value={active.value} {run} {problems} />
      {#if lib && tab === "lib"}
        <div class="publish">
          <button class="run" onclick={publish} disabled={lib.busy}>{lib.busy ? "Publishing" : "Publish new revision"}</button>
          <span>{current.lib?.name}: edit, publish, then pin the cell to the new hash.</span>
        </div>
        {#if lib.error}<pre class="liberr">{lib.error}</pre>{/if}
      {/if}
    </section>

    <section class="card stage">
      <div class="bar">
        <span class="file">{isScene ? "stage" : "result"}</span>
        <span class="spacer"></span>
        {#if reply?.ok}
          {#if cellHash}<span class="chip" title="Content hash of this cell and its pinned dependencies">cell <b>{short(cellHash)}</b></span>{/if}
          {#if reply.result.build}
            <span class="chip" title="Compiling the cell with rustc and linking it to wasm. 0 invocations means the build cache answered."><b>{reply.result.build.ms}</b> ms build{#if reply.result.build.rustc_invocations === 0}&nbsp;· cached{/if}</span>
          {/if}
          {#if reply.result.runtime_ms}
            <span class="chip" title="Validating and compiling the wasm for this machine. Skipped when the module is already compiled."><b>{reply.result.runtime_ms.compile_ms.toFixed(1)}</b> ms load{#if reply.result.runtime_ms.cache_hit}&nbsp;· cached{/if}</span>
            <span class="chip" title="Running the cell's entry function."><b>{reply.result.runtime_ms.run_ms.toFixed(2)}</b> ms run</span>
          {/if}
          <span class="chip" title="Whole request, browser to daemon and back"><b>{reply.wall_ms}</b> ms total</span>
          {#if current.animate && frameMs}<span class="chip"><b>{frameMs}</b> ms / frame</span>{/if}
        {/if}
      </div>
      {#if reply && !reply.ok}
        <ul class="errors">
          {#each diagnostics as d}<li><span class="where">{d.line}:{d.col}</span>{d.message}</li>{/each}
          {#if !diagnostics.length}<li>{reply.result?.error ?? JSON.stringify(reply)}</li>{/if}
        </ul>
      {:else}
        {#if isScene}
          <Stage {scene} bind:drawMs />
          <p class="effects">
            {#each counts as [op, n]}<span><b>{op}</b> × {n}</span>{/each}
            {#if !counts.length}running…{/if}
            <span class="spacer"></span><span>paint <b>{drawMs}</b> ms</span>
          </p>
        {:else}
          <pre class="value">{value === null ? "running…" : show(value)}</pre>
        {/if}
      {/if}
    </section>
  </div>

  {#if lib}
    <section class="card library">
      <div class="bar"><span class="file">{current.lib?.name}</span><span class="spacer"></span><span class="chip">{lib.revs.length} revision{lib.revs.length === 1 ? "" : "s"}</span></div>
      <div class="revs">
        {#each lib.revs as r, i}
          <button class="rev" class:pinned={r.hash === lib.pinned} onclick={() => pin(r.hash)}>
            <b>{short(r.hash)}</b>
            <span>{i === 0 ? "first" : r.changed.length ? `changed ${r.changed.join(", ")}` : "no item changed"}</span>
            {#if r.hash === lib.pinned}<em>pinned</em>{:else if r.hash === head}<em class="newer">newest · click to pin</em>{:else}<em class="newer">old · still runs</em>{/if}
          </button>
        {/each}
      </div>
      <p class="why">
        The cell depends on <code>surface @ {short(lib.pinned)}</code>. A definition is its content hash: renaming a
        local or reformatting keeps the hash, changing behaviour moves it, and a pinned hash keeps running after the
        name moves on. Cell hashes this session: {#each seen as h, i}<code class:now={i === 0}>{short(h)}</code>{/each}
      </p>
    </section>
  {/if}
</main>

<style>
  :global(:root) { --bg: #f6f5f2; --ink: #1d1d1b; --dim: #7b7972; --accent: #6b7cf0; color-scheme: light dark; }
  @media (prefers-color-scheme: dark) { :global(:root) { --bg: #0d0e13; --ink: #e8e9f0; --dim: #7d819a; --accent: #8d9bff; } }
  :global(body) { margin: 0; background: var(--bg); color: var(--ink); font: 16px/1.5 "Inter", system-ui, sans-serif; -webkit-font-smoothing: antialiased; }
  main { max-width: 1280px; margin: 0 auto; padding: 48px 20px 80px; }
  h1 { margin: 0; font-size: 34px; font-weight: 700; letter-spacing: -0.03em; }
  .lede { margin: 6px 0 0; color: var(--dim); max-width: 760px; }
  .ideas { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 12px; margin-top: 22px; }
  @media (max-width: 800px) { .ideas { grid-template-columns: minmax(0, 1fr); } }
  .ideas div { background: color-mix(in srgb, var(--ink) 5%, transparent); border-radius: 12px; padding: 14px 16px; }
  .ideas h3 { margin: 0 0 4px; font-size: 14px; }
  .ideas p { margin: 0; color: var(--dim); font-size: 13.5px; }
  code { font-family: "JetBrains Mono", ui-monospace, monospace; font-size: 0.9em; color: var(--ink); }
  nav { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; margin: 26px 0 14px; }
  nav button { border: 0; background: transparent; color: var(--dim); padding: 6px 12px; border-radius: 999px; cursor: pointer; font: inherit; font-size: 14px; }
  nav button:hover { color: var(--ink); }
  nav button.on { background: var(--ink); color: var(--bg); }
  .note { margin-left: 10px; color: var(--dim); font-size: 13px; }
  .grid { display: grid; grid-template-columns: minmax(0, 1.1fr) minmax(0, 1fr); gap: 16px; align-items: start; }
  @media (max-width: 1000px) { .grid { grid-template-columns: minmax(0, 1fr); } .stage { order: -1; } }
  .card { background: #14151c; color: #c8d0f0; border-radius: 14px; overflow: hidden; box-shadow: 0 0 0 1px rgba(255,255,255,0.05); }
  .bar { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; padding: 8px 14px; border-bottom: 1px solid rgba(255,255,255,0.06); font-size: 12.5px; min-height: 30px; }
  .file { color: #7a86b8; font-family: "JetBrains Mono", ui-monospace, monospace; }
  .tab { border: 0; background: transparent; color: #545b7f; font: 12.5px "JetBrains Mono", ui-monospace, monospace; padding: 4px 8px; border-radius: 6px; cursor: pointer; }
  .tab.on { color: #c8d0f0; background: rgba(255,255,255,0.06); }
  .spacer { flex: 1; }
  .warn { color: #ff9e64; }
  kbd { color: #545b7f; font: 12px "JetBrains Mono", monospace; }
  .run { border: 0; background: #7aa2f7; color: #0d0e13; padding: 5px 16px; border-radius: 8px; font: 600 13px Inter, system-ui, sans-serif; cursor: pointer; }
  .run:disabled { opacity: .55; cursor: progress; }
  .chip { white-space: nowrap; color: #7a86b8; background: rgba(255,255,255,0.05); padding: 2px 9px; border-radius: 999px; font-variant-numeric: tabular-nums; }
  .chip b { color: #c8d0f0; font-weight: 600; }
  .value { margin: 0; padding: 18px 20px; min-height: 120px; font: 15px/1.6 "JetBrains Mono", ui-monospace, monospace; color: #9ece6a; white-space: pre-wrap; word-break: break-word; }
  .effects { display: flex; gap: 14px; flex-wrap: wrap; margin: 0; padding: 10px 16px 14px; color: #545b7f; font: 12px "JetBrains Mono", ui-monospace, monospace; }
  .effects b { color: #7a86b8; font-weight: 500; }
  .errors { list-style: none; margin: 0; padding: 16px; font: 13.5px/1.65 "JetBrains Mono", ui-monospace, monospace; color: #f7768e; min-height: 200px; }
  .errors li { margin-bottom: 6px; }
  .where { color: #545b7f; margin-right: 12px; }
  .publish { display: flex; align-items: center; gap: 12px; padding: 10px 14px; border-top: 1px solid rgba(255,255,255,0.06); color: #7a86b8; font-size: 12.5px; }
  .liberr { margin: 0; padding: 10px 16px 14px; color: #f7768e; font: 12.5px/1.6 "JetBrains Mono", ui-monospace, monospace; white-space: pre-wrap; }
  .library { margin-top: 16px; }
  .revs { display: flex; gap: 10px; flex-wrap: wrap; padding: 14px; }
  .rev { display: flex; flex-direction: column; gap: 2px; align-items: flex-start; text-align: left; border: 1px solid rgba(255,255,255,0.08); background: transparent; color: #7a86b8; padding: 8px 12px; border-radius: 10px; cursor: pointer; font: 12.5px "JetBrains Mono", ui-monospace, monospace; }
  .rev b { color: #c8d0f0; font-weight: 500; }
  .rev em { font-style: normal; color: #545b7f; }
  .rev.pinned { border-color: #7aa2f7; }
  .rev.pinned em { color: #7aa2f7; }
  .rev .newer { color: #ff9e64; }
  .why { margin: 0; padding: 0 16px 16px; color: #7a86b8; font-size: 13px; }
  .why code { font: 12px "JetBrains Mono", ui-monospace, monospace; color: #c8d0f0; margin: 0 4px; }
  .why code.now { color: #7aa2f7; }
</style>
