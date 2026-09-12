<script lang="ts">
  import { onMount } from 'svelte';
  import { Search } from 'lucide-svelte';
  import { searchDocuments } from '$lib/data/search';
  let { data } = $props();
  let query = $state('');
  let input: HTMLInputElement;
  onMount(() => { input.focus(); });
  const results = $derived(searchDocuments(data.documents, query));
</script>
<svelte:head><title>Search · Loom</title><meta name="description" content="Search Loom documentation headings and paragraphs." /></svelte:head>
<div class="eyebrow">REPOSITORY INDEX</div><h1>Search documentation</h1>
<label class="search-field"><Search size={18} /><input bind:this={input} bind:value={query} type="search" aria-label="Search headings and paragraphs" placeholder="Search headings and paragraphs…" /></label>
<p class="search-summary" aria-live="polite">{query.trim() ? `${results.length} matches` : `${data.documents.length} documents indexed locally. Type to search.`}</p>
<div class="search-results">{#each results as result}<a href={result.href}><span class="result-kind">{result.documentTitle} / {result.kind}</span><h2>{result.title}</h2><p>{result.excerpt}</p></a>{/each}</div>
{#if query.trim() && !results.length}<div class="empty-state"><h2>No matching documentation</h2><p>Try a shorter phrase, a crate name, or a term such as actor, effect, or handler.</p></div>{/if}
