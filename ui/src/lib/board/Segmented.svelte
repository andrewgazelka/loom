<script lang="ts">
  /** Our own segmented control: one button per option, arrow keys move, luminance marks the choice. */
  let {
    options,
    value,
    label,
    onchange,
  }: {
    options: { value: string; label: string; count?: number | null }[];
    value: string;
    label: string;
    onchange: (value: string) => void;
  } = $props();
  function keyboard(event: KeyboardEvent, index: number) {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
    event.preventDefault();
    const next = options[(index + (event.key === "ArrowRight" ? 1 : options.length - 1)) % options.length];
    if (!next) return;
    onchange(next.value);
    (event.currentTarget as HTMLElement).parentElement
      ?.querySelector<HTMLElement>(`[data-tab="${next.value}"]`)
      ?.focus();
  }
</script>

<div class="segmented" role="tablist" aria-label={label}>
  {#each options as option, index (option.value)}
    <button
      type="button"
      role="tab"
      data-tab={option.value}
      aria-selected={option.value === value}
      tabindex={option.value === value ? 0 : -1}
      onclick={() => onchange(option.value)}
      onkeydown={(event) => keyboard(event, index)}
      >{option.label}{#if option.count !== undefined && option.count !== null}<span class="count"
          >{option.count}</span
        >{/if}</button
    >
  {/each}
</div>

<style>
  .segmented {
    display: inline-flex;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--side);
    padding: 2px;
    gap: 2px;
  }
  button {
    padding: 3px 10px;
    border-radius: 4px;
    color: var(--muted);
    white-space: nowrap;
  }
  button:hover {
    background: var(--selection);
    color: var(--ink);
  }
  button[aria-selected="true"] {
    background: var(--card);
    color: var(--ink);
    box-shadow: inset 0 0 0 1px var(--line);
  }
  .count {
    margin-left: 6px;
    font-family: var(--mono);
    font-size: 0.86em;
    color: var(--muted);
  }
</style>
