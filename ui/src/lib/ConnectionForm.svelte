<script lang="ts">
  import { ChevronRight } from "lucide-svelte";
  export let endpoint = "";
  export let token = "";
  export let session = "";
  export let connecting = false;
  export let submit: () => Promise<void>;
</script>

<form on:submit|preventDefault={submit}>
  <label>Bearer token<input
    bind:value={token}
    type="password"
    required
    disabled={connecting}
    autocomplete="off"
    data-1p-ignore
    data-lpignore="true"
    placeholder="Paste your Loom token"
    aria-describedby="token-help"
  /></label>
  <p id="token-help" class="token-help">Use the contents of the token file shown when you start <code>nix run .</code>.</p>
  <details>
    <summary><ChevronRight size={11} /> Connection options</summary>
    <label>API endpoint<input bind:value={endpoint} disabled={connecting} placeholder="Same origin" type="url" /></label>
    <label>Session<input bind:value={session} disabled={connecting} placeholder="Created on first evaluation" /></label>
  </details>
  <div class="connection-footer">
    <p>Saved in this browser after verification.</p>
    <button class="primary" disabled={connecting || !token.trim()}>{connecting ? "Verifying…" : "Connect to Loom"}</button>
  </div>
</form>

<style>
  .token-help { color:var(--muted); font-size:12px; margin:8px 0 22px; }
  .token-help code { font:11px var(--mono); }
</style>
