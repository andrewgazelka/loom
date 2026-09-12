<script lang="ts">
  import '@fontsource/inter/400.css';
  import '@fontsource/inter/500.css';
  import '@fontsource/inter/600.css';
  import '@fontsource/inter/700.css';
  import '$lib/ui/site.css';
  import Shell from '$lib/ui/Shell.svelte';
  let { data, children } = $props();
</script>
<Shell nav={data.nav}>{@render children()}</Shell>
