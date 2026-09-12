<script lang="ts">
  import Article from '$lib/ui/Article.svelte';
  let { data } = $props();
</script>
<svelte:head><title>{data.doc.title} · Loom</title><meta name="description" content={data.doc.description} /></svelte:head>
{#key data.doc.slug}<Article doc={data.doc} />{/key}
