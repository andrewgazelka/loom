<script lang="ts">
  import { GitBranch, Settings, RefreshCw, HelpCircle } from "lucide-svelte";
  export let connected: boolean;
  export let refreshing: boolean;
  export let refresh: () => Promise<void>;
  export let toggleHelp: () => void;
  export let toggleSettings: () => void;
</script>

<header class="workspace-header">
  <a href="/" class="wordmark">loom</a><span class="header-divider">/</span>
  <div class="workspace-identity">
    <GitBranch size={14} /><span>workspace</span>
  </div>
  <div class="header-controls">
    <span class="connection-status"
      ><span class:connected class="status-dot"></span>{connected
        ? "Connected"
        : "Offline"}</span
    ><button
      title="Refresh workspace"
      aria-label="Refresh workspace"
      disabled={refreshing}
      on:click={refresh}><RefreshCw size={14} /></button
    ><button
      title="Keyboard & gestures"
      aria-label="Keyboard & gestures"
      on:click={toggleHelp}><HelpCircle size={14} /></button
    ><button
      title="Connection settings"
      aria-label="Connection settings"
      on:click={toggleSettings}><Settings size={14} /></button
    >
  </div>
</header>
