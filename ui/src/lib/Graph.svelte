<script lang="ts">
  import type { Definition } from "./api";
  export let definitions: Definition[] = [];
  export let edges: { from: string; to: string }[] = [];
  let x = 45,
    y = 35,
    scale = 1;
  function label(definition: Definition): string {
    const name =
      definition.name_hint || definition.name || definition.hash.slice(0, 16);
    return name.length > 23 ? `${name.slice(0, 20)}…` : name;
  }
  $: nodes = definitions.map((def, index) => ({
    ...def,
    x: (index % 3) * 260,
    y: Math.floor(index / 3) * 125,
  }));
  function wheel(event: WheelEvent) {
    event.preventDefault();
    if (event.ctrlKey || event.metaKey) {
      const box = event.currentTarget as HTMLElement;
      const bounds = box.getBoundingClientRect();
      const px = event.clientX - bounds.left,
        py = event.clientY - bounds.top;
      const next = Math.min(
        3,
        Math.max(0.25, scale * Math.exp(-event.deltaY * 0.01)),
      );
      x = px - ((px - x) * next) / scale;
      y = py - ((py - y) * next) / scale;
      scale = next;
    } else {
      x -= event.deltaX;
      y -= event.deltaY;
    }
  }
  function keyboard(event: KeyboardEvent) {
    const step = 40;
    if (event.key === "ArrowLeft") x += step;
    else if (event.key === "ArrowRight") x -= step;
    else if (event.key === "ArrowUp") y += step;
    else if (event.key === "ArrowDown") y -= step;
    else if (event.key === "0") {
      x = 45;
      y = 35;
      scale = 1;
    } else return;
    event.preventDefault();
  }
</script>

<!-- This application region implements keyboard navigation and pointer gestures. -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex a11y_no_noninteractive_element_interactions -->
<div
  class="graph"
  role="application"
  aria-label="Definition dependency graph. Scroll to pan, pinch to zoom. Arrow keys pan, zero resets."
  tabindex="0"
  on:wheel|nonpassive={wheel}
  on:keydown={keyboard}
>
  <svg width="100%" height="100%" aria-label="Definition dependencies"
    ><g transform={`translate(${x},${y}) scale(${scale})`}>
      {#each edges as edge}{@const from = nodes.find(
          (n) => n.hash === edge.from,
        )}{@const to = nodes.find(
          (n) => n.hash === edge.to,
        )}{#if from && to}<path
            d={`M${from.x + 210},${from.y + 35} C${from.x + 245},${from.y + 35} ${to.x - 30},${to.y + 35} ${to.x},${to.y + 35}`}
            fill="none"
            stroke="var(--muted)"
            stroke-width="1.5"
          />{/if}{/each}
      {#each nodes as node}<g transform={`translate(${node.x},${node.y})`}
          ><rect
            width="210"
            height="72"
            rx="9"
            fill="var(--card)"
            stroke="var(--line)"
          /><text x="14" y="25" fill="var(--ink)" font-size="13"
            ><title>{node.name_hint || node.name || node.hash}</title>{label(
              node,
            )}</text
          ><text x="14" y="50" fill="var(--muted)" font-size="11"
            >{node.lang.toUpperCase()} · {node.hash.slice(0, 14)}</text
          ></g
        >{/each}
    </g></svg
  >
  {#if !nodes.length}<div class="empty">
      Define a function to start your graph.
    </div>{/if}
</div>

<style>
  .graph {
    position: relative;
    min-height: 420px;
    height: 60vh;
    overflow: hidden;
    overscroll-behavior: contain;
    background-image: radial-gradient(var(--line) 1px, transparent 1px);
    background-size: 20px 20px;
    border: 1px solid var(--line);
    border-radius: 10px;
  }
  .empty {
    position: absolute;
    top: 45%;
    width: 100%;
    text-align: center;
    color: var(--muted);
    pointer-events: none;
  }
  svg text {
    font-family: var(--mono);
  }
</style>
