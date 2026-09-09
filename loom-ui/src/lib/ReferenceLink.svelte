<script lang="ts">
  import { short } from "./api";
  export let hash: string;
  export let inspect: (hash: string) => void;
  export let length = 24;
</script>

<button class="reference-link" title={hash} on:click={() => inspect(hash)}
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
  .reference-link:hover {
    text-decoration: underline;
  }
</style>
