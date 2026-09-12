<script lang="ts">
  import { onMount } from 'svelte';
  import type { DocPage } from '$lib/data/docs';
  let { doc, embedded = false }: { doc: DocPage; embedded?: boolean } = $props();
  let article: HTMLElement;
  let mounted = $state(false);
  let diagramError = $state('');
  onMount(() => { mounted = true; });
  $effect(() => {
    const html = doc.html;
    if (!mounted || !article || !html) return;
    let cancelled = false;
    const container = article;
    diagramError = '';
    void (async () => {
      const nodes = Array.from(container.querySelectorAll<HTMLElement>('.mermaid'));
      if (!nodes.length) return;
      try {
        const { default: mermaid } = await import('mermaid');
        if (cancelled) return;
        mermaid.initialize({ startOnLoad: false, theme: 'dark', securityLevel: 'strict', fontFamily: 'Inter, sans-serif' });
        await mermaid.run({ nodes });
      } catch (error) { if (!cancelled) diagramError = `Diagram could not render: ${error instanceof Error ? error.message : String(error)}`; }
    })();
    return () => { cancelled = true; };
  });
</script>
{#if !embedded}<div class="document-meta"><span>{doc.source}</span><span>{doc.wordCount.toLocaleString()} words · {Math.max(1, Math.ceil(doc.wordCount / 220))} min read</span></div>{/if}
{#if diagramError}<p role="alert" class="diagram-error">{diagramError}</p>{/if}
<article class="prose" class:embedded bind:this={article}>{@html doc.html}</article>
