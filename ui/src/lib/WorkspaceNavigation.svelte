<script lang="ts">
  import { Terminal, AlignLeft, Circle, Database } from "lucide-svelte";
  import type { WorkspaceView } from "./workspace-view";
  export let view: WorkspaceView;
  export let sequence: number;
  export let definitionCount: number;
  export let actorCount: number;
  export let navigate: (view: WorkspaceView) => void;
  const views: WorkspaceView[] = ["Session", "Definitions", "Actors", "CAS", "Effects"];
</script>

  <nav class="primary-nav" aria-label="Workspace views">
    {#each views as item}<button
        aria-current={view === item ? "page" : undefined}
        on:click={() => navigate(item)}
        >{#if item === "Session"}<Terminal
            size={14}
          />{:else if item === "Definitions"}<AlignLeft
            size={14}
          />{:else if item === "Actors"}<Circle size={13} />{:else}<Database
            size={14}
          />{/if}<span>{item}</span
        >{#if item === "Definitions" || item === "Actors"}<small
            >{item === "Definitions"
              ? definitionCount
              : actorCount}</small
          >{/if}</button
      >{/each}<span class="sequence">seq {sequence}</span>
  </nav>
