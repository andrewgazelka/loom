<script context="module" lang="ts">
  let nextId = 0;
</script>
<script lang="ts">
  import { ChevronDown, Check } from "lucide-svelte";
  import { createEventDispatcher, tick } from "svelte";
  export let value = "";
  export let label: string;
  export let options: { value: string; label: string }[];
  export let placeholder = "Choose…";
  const dispatch = createEventDispatcher<{ change: string }>();
  const id = `select-${++nextId}`;
  let open = false, active = 0;
  let root: HTMLDivElement;
  let menu: HTMLDivElement;
  async function placeMenu() {
    await tick();
    if (!open || !menu) return;
    menu.showPopover();
    const anchor = root.getBoundingClientRect();
    const height = Math.min(menu.scrollHeight, 260);
    const below = window.innerHeight - anchor.bottom;
    const top = below >= height + 12 ? anchor.bottom + 6 : Math.max(8, anchor.top - height - 6);
    menu.style.top = `${top}px`;
    menu.style.left = `${Math.max(8, Math.min(anchor.left, window.innerWidth - menu.offsetWidth - 8))}px`;
    menu.querySelector<HTMLElement>(`[id="${id}-${active}"]`)?.scrollIntoView({block:"nearest"});
  }
  $: if (open) { active; void placeMenu(); }

  function show() { active = Math.max(0, options.findIndex(option => option.value === value)); open = true; }
  function choose(index: number) {
    const option = options[index];
    if (!option) return;
    value = option.value; open = false; dispatch("change", value);
  }
  function keydown(event: KeyboardEvent) {
    if (["ArrowDown", "ArrowUp", "Home", "End", "Enter", " ", "Escape"].includes(event.key)) event.preventDefault();
    if (event.key === "Escape" || event.key === "Tab") { open = false; return; }
    if (event.key === "Enter" || event.key === " ") { if (open) choose(active); else show(); return; }
    if (["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) {
      if (!open) show();
      else if (event.key === "ArrowDown") active = Math.min(options.length - 1, active + 1);
      else if (event.key === "ArrowUp") active = Math.max(0, active - 1);
      if (event.key === "Home") active = 0;
      if (event.key === "End") active = options.length - 1;
    }
  }
</script>
<svelte:window on:resize={() => { if (open) void placeMenu(); }} on:pointerdown={(event) => { if (!root?.contains(event.target as Node)) open = false; }} />
<div class="select" bind:this={root}>
  <button type="button" class="trigger" role="combobox" aria-label={label} aria-expanded={open} aria-controls={id} aria-haspopup="listbox" aria-activedescendant={open ? `${id}-${active}` : undefined} on:keydown={keydown} on:click={() => open ? open = false : show()}>
    <span>{options.find(option => option.value === value)?.label ?? placeholder}</span><ChevronDown size={12} />
  </button>
  {#if open}<div class="options" popover="manual" bind:this={menu} role="listbox" {id} aria-label={label}>
    {#each options as option, index}<button type="button" role="option" id={`${id}-${index}`} aria-selected={option.value === value} class:active={index === active} tabindex="-1" on:pointerdown|preventDefault on:click={() => choose(index)}>
      <span>{option.label}</span>{#if option.value === value}<Check size={12} />{/if}
    </button>{/each}
  </div>{/if}
</div>
<style>
  .select { position: relative; min-width: 0; }
  .trigger { display:flex; align-items:center; gap:12px; padding:5px 0; font:inherit; color:var(--ink); text-align:left; }
  .trigger span { overflow:hidden; text-overflow:ellipsis; }
  .options { position:fixed; margin:0; z-index:30; min-width:160px; max-width:350px; max-height:260px; overflow:auto; padding:4px; border:1px solid var(--line); border-radius:7px; background:var(--card); box-shadow:0 8px 24px #0002; }
  .options button { width:100%; display:flex; align-items:center; justify-content:space-between; gap:16px; text-align:left; padding:8px 10px; border-radius:4px; font:11px var(--mono); color:var(--ink); }
  .options button.active, .options button:hover { background:var(--code); }
</style>
