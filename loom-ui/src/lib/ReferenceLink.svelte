<script lang="ts">
  import { short } from "./api";
  export let hash: string;
  export let inspect: (hash: string) => void;
  export let length = 24;
  export let subtle = false;
</script>

<button class="reference-link" class:subtle title={hash} on:click={() => inspect(hash)}
  >{short(hash, length)} ↗</button
>

<style>
  .reference-link {
    font: inherit;
    text-align: left;
    padding: 0;
    color: var(--link);
    overflow-wrap: anywhere;
  }
  .reference-link.subtle {
    font: 9px var(--mono);
    color: var(--muted);
  }
  .reference-link:hover {
    text-decoration: underline;
  }
</style>
