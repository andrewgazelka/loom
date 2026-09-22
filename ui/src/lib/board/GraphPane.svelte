<script lang="ts">
  import { Maximize2, Share2 } from "lucide-svelte";
  import type { Model } from "./feed";
  import { NODE_HEIGHT, NODE_WIDTH, type Layout } from "./layout";
  let {
    model,
    graph,
    selected,
    onselect,
  }: {
    model: Model;
    graph: Layout;
    selected: string;
    onselect: (hash: string) => void;
  } = $props();
  let width = $state(0);
  let height = $state(0);
  let x = $state(24);
  let y = $state(24);
  let scale = $state(1);
  // Until the reader pans or zooms, the view keeps fitting the whole graph
  // as definitions arrive; a single node is never enlarged past natural size.
  let userMoved = false;
  let drag: { px: number; py: number; ox: number; oy: number } | null = null;

  function fit() {
    if (!graph.width || !graph.height || !width || !height) return;
    const pad = 28;
    const next = Math.min(
      1,
      Math.max(
        0.1,
        Math.min(
          (width - 2 * pad) / graph.width,
          (height - 2 * pad) / graph.height,
        ),
      ),
    );
    scale = next;
    x = (width - graph.width * next) / 2;
    y = (height - graph.height * next) / 2;
  }
  $effect(() => {
    // Reads graph.nodes.length, graph.width, graph.height, width and height so a
    // new definition or a resized pane refits until the reader takes over.
    void [graph.nodes.length, graph.width, graph.height, width, height];
    if (!userMoved && graph.nodes.length && width && height) fit();
  });
  function wheel(event: WheelEvent) {
    userMoved = true;
    event.preventDefault();
    const box = (event.currentTarget as HTMLElement).getBoundingClientRect();
    const px = event.clientX - box.left;
    const py = event.clientY - box.top;
    const next = Math.min(
      4,
      Math.max(0.1, scale * Math.exp(-event.deltaY * 0.0015)),
    );
    x = px - ((px - x) * next) / scale;
    y = py - ((py - y) * next) / scale;
    scale = next;
  }
  /** Event attributes for wheel are passive in Svelte 5; zooming needs preventDefault. */
  function zoomable(node: HTMLElement) {
    node.addEventListener("wheel", wheel, { passive: false });
    return {
      destroy() {
        node.removeEventListener("wheel", wheel);
      },
    };
  }
  function down(event: PointerEvent) {
    if (event.button !== 0) return;
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    drag = { px: event.clientX, py: event.clientY, ox: x, oy: y };
  }
  function move(event: PointerEvent) {
    if (!drag) return;
    userMoved = true;
    x = drag.ox + event.clientX - drag.px;
    y = drag.oy + event.clientY - drag.py;
  }
  function up() {
    drag = null;
  }
  function keyboard(event: KeyboardEvent) {
    const step = 40;
    if (event.key === "ArrowLeft") x += step;
    else if (event.key === "ArrowRight") x -= step;
    else if (event.key === "ArrowUp") y += step;
    else if (event.key === "ArrowDown") y -= step;
    else if (event.key === "0") {
      userMoved = false;
      fit();
    }
    else return;
    event.preventDefault();
  }
  function truncate(label: string): string {
    return label.length > 22 ? `${label.slice(0, 20)}…` : label;
  }
</script>

<div class="section-bar">
  <Share2 size={14} class="icon-item" />
  <h2>Graph</h2>
  <span class="muted">{graph.nodes.length} nodes · {graph.edges.length} edges</span>
  <span class="legend muted"
    ><i class="key static"></i>static <i class="key isolated"></i>isolated call
    <i class="key host"></i>host</span
  >
  <button
    class="push"
    type="button"
    aria-label="Fit to content"
    title="Fit to content (0)"
    onclick={() => {
      userMoved = false;
      fit();
    }}><Maximize2 size={13} /></button
  >
</div>
<!-- svelte-ignore a11y_no_noninteractive_tabindex a11y_no_noninteractive_element_interactions -->
<div
  class="canvas"
  data-pane="graph"
  role="application"
  tabindex="0"
  aria-label="Definition graph. Drag to pan, wheel to zoom, arrows pan, zero fits."
  bind:clientWidth={width}
  bind:clientHeight={height}
  use:zoomable
  onpointerdown={down}
  onpointermove={move}
  onpointerup={up}
  onpointercancel={up}
  onkeydown={keyboard}
>
  <svg width="100%" height="100%" aria-label="Definitions and their edges">
    <g transform={`translate(${x},${y}) scale(${scale})`}>
      {#each graph.edges as edge (`${edge.kind}\n${edge.from}\n${edge.to}`)}
        <path
          class={`edge ${edge.kind}`}
          data-kind={edge.kind}
          data-from={edge.from}
          data-to={edge.to}
          d={edge.path}
        />
      {/each}
      {#each graph.nodes as node (node.id)}
        <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
        <g
          class="node"
          class:synthetic={node.synthetic}
          class:active={model.activeUntil[node.id] !== undefined}
          class:selected={selected === node.id}
          data-node={node.id}
          transform={`translate(${node.x},${node.y})`}
          onclick={() => {
            if (!node.synthetic) onselect(node.id);
          }}
        >
          <rect width={NODE_WIDTH} height={NODE_HEIGHT} rx="6" />
          <text class="label" x="10" y={node.detail ? 17 : NODE_HEIGHT / 2 + 4}
            ><title>{node.label}</title>{truncate(node.label)}</text
          >
          {#if node.detail}<text class="detail" x="10" y="33">{node.detail}</text
            >{/if}
        </g>
      {/each}
    </g>
  </svg>
  {#if !graph.nodes.length}<div class="empty overlay">
      No definitions to draw.
    </div>{/if}
</div>

<style>
  .canvas {
    position: relative;
    flex: 1;
    min-height: 0;
    overflow: hidden;
    overscroll-behavior: contain;
    background-image: radial-gradient(var(--line) 1px, transparent 1px);
    background-size: 20px 20px;
    cursor: grab;
    touch-action: none;
    outline: none;
  }
  .canvas:focus-visible {
    outline: 1px solid var(--focus);
    outline-offset: -1px;
  }
  .canvas:active {
    cursor: grabbing;
  }
  .overlay {
    position: absolute;
    top: 45%;
    width: 100%;
    pointer-events: none;
  }
  .legend {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 0.82em;
    margin-left: 10px;
  }
  .key {
    display: inline-block;
    width: 16px;
    border-top: 1.5px solid var(--muted);
    margin-left: 6px;
  }
  .key.isolated {
    border-top: 1.5px dashed var(--verdict-red);
  }
  .key.host {
    border-top: 1.5px dotted var(--muted);
  }
  svg {
    display: block;
    stroke-width: 1;
  }
  .edge {
    fill: none;
    stroke: var(--muted);
    stroke-width: 1.4;
  }
  .edge.isolated {
    stroke: var(--verdict-red);
    stroke-dasharray: 6 4;
  }
  .edge.host {
    stroke: var(--muted);
    stroke-width: 1;
    stroke-dasharray: 2 3;
  }
  .node rect {
    fill: var(--card);
    stroke: var(--line);
  }
  .node:not(.synthetic) {
    cursor: pointer;
  }
  .node.selected rect {
    fill: var(--selection);
  }
  .node.synthetic rect {
    fill: var(--side);
    stroke-dasharray: 3 3;
  }
  .node.active rect {
    stroke: var(--ink);
    stroke-width: 2;
  }
  .node text {
    fill: var(--ink);
    font-family: var(--sans);
    font-size: 12px;
    pointer-events: none;
  }
  .node .detail {
    fill: var(--muted);
    font-family: var(--mono);
    font-size: 10px;
  }
  .node.synthetic text {
    fill: var(--muted);
  }
</style>
