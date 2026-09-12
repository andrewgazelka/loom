<script lang="ts">
  import Article from '$lib/ui/Article.svelte';
  import { ArrowRight, Boxes, Terminal, BookOpen } from 'lucide-svelte';
  let { data } = $props();
</script>
<svelte:head><title>Loom · Documentation</title><meta name="description" content={data.readme?.description ?? 'Loom repository documentation, architecture, crates, and MCP tools.'} /></svelte:head>
<div class="eyebrow">LOOM / DEVELOPER DOCUMENTATION</div>
<h1 id="overview" class="overview-title">Loom</h1>
<p class="overview-description">{data.readme?.description ?? 'Repository documentation and generated reference.'}</p>
<div class="overview-actions"><a class="primary-button" href="/docs/guide/">Read the guide <ArrowRight size={14} /></a><a class="secondary-button" href="/docs/actors-turso/">Explore actors <ArrowRight size={14} /></a></div>
<h2 id="in-this-repository" class="section-heading">In this repository</h2>
<div class="reference-grid">
  <a href="/docs/readme/"><BookOpen color="#c0ad83" size={19} /><span class="card-count">{data.docCount}</span><strong>Documents</strong><span>Guides and design decisions</span><ArrowRight class="card-arrow" size={14} /></a>
  <a href="/crates/"><Boxes color="#9f91cc" size={19} /><span class="card-count">{data.crateCount}</span><strong>Crates</strong><span>Generated package reference</span><ArrowRight class="card-arrow" size={14} /></a>
  <a href="/mcp/"><Terminal color="#96b9b3" size={19} /><span class="card-count">{data.toolCount}</span><strong>MCP tools</strong><span>Actor control interface</span><ArrowRight class="card-arrow" size={14} /></a>
</div>
{#if data.readme}<div class="source-heading"><h2 id="readme">From the README</h2><a href="/docs/readme/">Full document <ArrowRight size={13} /></a></div><Article doc={data.readme} embedded />{/if}
{#if data.architecture}<div class="source-heading"><h2 id="architecture">Architecture</h2><a href="/docs/architecture/">Full document <ArrowRight size={13} /></a></div><Article doc={data.architecture} embedded />{/if}
