<script lang="ts">
  import { onMount, tick } from "svelte";
  import { Search, FileCode2, Network } from "lucide-svelte";
  import { commands } from "./commands";
  export let choose: (id: string) => void;
  export let close: () => void;
  let dialog: HTMLDialogElement;
  let input: HTMLInputElement;
  let query = "",
    active = 0;
  $: filtered = commands.filter((command) =>
    `${command.name} ${command.description}`
      .toLowerCase()
      .includes(query.toLowerCase()),
  );
  $: (query, (active = 0));
  onMount(() => {
    const prior = document.activeElement as HTMLElement | null;
    dialog.showModal();
    input.focus();
    return () => prior?.focus();
  });
  async function keyboard(event: KeyboardEvent) {
    if (
      event.key === "ArrowDown" ||
      event.key === "ArrowUp" ||
      (event.ctrlKey && ["j", "k"].includes(event.key))
    ) {
      event.preventDefault();
      active = Math.max(
        0,
        Math.min(
          filtered.length - 1,
          active + (["ArrowDown", "j"].includes(event.key) ? 1 : -1),
        ),
      );
      await tick();
      document
        .getElementById(`command-${active}`)
        ?.scrollIntoView({ block: "nearest" });
    } else if (event.key === "Enter") {
      event.preventDefault();
      if (filtered[active]) choose(filtered[active]!.id);
    }
  }
</script>

<dialog
  bind:this={dialog}
  on:cancel={close}
  on:close={close}
  aria-label="Command palette"
>
  <div class="palette-search">
    <Search size={16} /><input
      bind:this={input}
      bind:value={query}
      on:keydown={keyboard}
      placeholder="Type a command…"
      aria-label="Find a command"
      role="combobox"
      aria-expanded="true"
      aria-controls="command-list"
      aria-activedescendant={filtered.length ? `command-${active}` : undefined}
    /><button on:click={close}>Esc</button>
  </div>
  <div
    id="command-list"
    class="command-list"
    role="listbox"
    aria-label="Commands"
  >
    {#each filtered as command, index}<button
        id={`command-${index}`}
        role="option"
        aria-selected={index === active}
        tabindex="-1"
        on:click={() => choose(command.id)}
      >
        {#if command.group === "Definitions"}<FileCode2
            size={16}
            class="icon-code"
          />{:else}<Network size={16} class="icon-actor" />{/if}<span
          ><strong>{command.name}</strong><small>{command.description}</small
          ></span
        ><span class="group">{command.group}</span>
      </button>{:else}<p class="empty">No matching commands.</p>{/each}
  </div>
</dialog>

<style>
  dialog {
    padding: 0;
    top: 12vh;
    margin: 0 auto;
    width: min(640px, 92vw);
    max-height: 72vh;
    border: 1px solid var(--focus);
    border-radius: 6px;
    color: var(--ink);
    background: var(--card);
  }
  dialog::backdrop {
    background: #0008;
  }
  .palette-search {
    display: flex;
    align-items: center;
    padding: 12px;
    gap: 12px;
    border-bottom: 1px solid var(--line);
  }
  .palette-search input {
    border: 0;
    flex: 1;
    min-width: 0;
    background: transparent;
  }
  .palette-search button {
    color: var(--muted);
  }
  .command-list {
    max-height: 58vh;
    overflow: auto;
    padding: 5px;
  }
  .command-list > button {
    display: flex;
    align-items: center;
    gap: 12px;
    width: 100%;
    text-align: left;
    padding: 9px;
    border-radius: 4px;
  }
  .command-list > button[aria-selected="true"] {
    background: var(--selection);
  }
  strong {
    display: block;
    font-weight: 500;
  }
  small {
    display: block;
    color: var(--muted);
    margin-top: 3px;
  }
  .group {
    margin-left: auto;
    font-size: 0.88em;
    color: var(--muted);
  }
</style>
