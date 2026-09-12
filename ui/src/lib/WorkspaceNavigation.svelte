<script lang="ts">
  import { Terminal, AlignLeft, Database } from "lucide-svelte";
  import type { WorkspaceView } from "./workspace-view";
  export let view: WorkspaceView;
  export let sequence: number;
  export let definitionCount: number;
  export let navigate: (view: WorkspaceView) => void;
  const views: WorkspaceView[] = ["Session", "Definitions", "CAS", "Effects"];
</script>

  <nav class="primary-nav" aria-label="Workspace views">
    {#each views as item}<button
        aria-current={view === item ? "page" : undefined}
        on:click={() => navigate(item)}
        >{#if item === "Session"}<Terminal
            size={14}
          />{:else if item === "Definitions"}<AlignLeft
            size={14}
          />{:else}<Database
            size={14}
          />{/if}<span>{item}</span
        >{#if item === "Definitions"}<small
            >{definitionCount}</small
          >{/if}</button
      >{/each}<span class="sequence">seq {sequence}</span>
  </nav>
