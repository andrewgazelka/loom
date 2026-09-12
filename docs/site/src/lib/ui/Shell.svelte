<script lang="ts">
  import { page } from '$app/state';
  import { goto } from '$app/navigation';
  import { BookOpen, Search, Boxes, Workflow, Cpu, FileCode2, Layers, Terminal, PanelLeft } from 'lucide-svelte';
  import type { Snippet } from 'svelte';
  import type { NavSection } from '$lib/data/nav';
  import type { DocPage } from '$lib/data/docs';
  let { nav, children }: { nav: NavSection[]; children: Snippet } = $props();
  let help = $state(false);
  let scale = $state(14);
  let mobileNav = $state(false);
  let shell: HTMLDivElement;
  let helpDialog = $state<HTMLDialogElement>();
  $effect(() => { if (!helpDialog) return; if (help) helpDialog.showModal(); else helpDialog.close(); });
  const icons = [BookOpen, Layers, Boxes, Workflow, FileCode2, Terminal, Cpu, BookOpen];
  const colors = ['#c0ad83', '#9f91cc', '#c4a276', '#91b8a0', '#c393ae', '#96b9b3', '#c3b783', '#a3abb9'];
  const outline = $derived((page.data.doc as DocPage | undefined)?.headings ?? (page.url.pathname === '/' ? [{ id: 'overview', text: 'Overview', level: 1 }, { id: 'in-this-repository', text: 'In this repository', level: 2 }, { id: 'readme', text: 'From the README', level: 2 }, ...(page.data.architecture ? [{ id: 'architecture', text: 'Architecture', level: 2 }] : [])] : []));
  const title = $derived((page.data.doc as DocPage | undefined)?.title ?? (page.url.pathname.startsWith('/crates') ? 'Crates' : page.url.pathname.startsWith('/mcp') ? 'MCP tools' : page.url.pathname.startsWith('/search') ? 'Search' : 'Overview'));
  function keydown(event: KeyboardEvent) {
    const target = event.target as HTMLElement;
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') { event.preventDefault(); help = false; void goto('/search/').then(() => shell.querySelector<HTMLInputElement>('input[type="search"]')?.focus()); return; }
    if (event.key === 'Escape') { help = false; mobileNav = false; return; }
    if (help && event.key !== '?') return;
    if (target.closest('input, textarea, select, [contenteditable="true"]') || event.metaKey || event.ctrlKey || event.altKey) return;
    if (event.key === '?') { help = !help; event.preventDefault(); }
    if (event.key === '+' || event.key === '=') { scale = Math.min(18, scale + 1); event.preventDefault(); }
    if (event.key === '-') { scale = Math.max(12, scale - 1); event.preventDefault(); }
    const panes = Array.from(shell.querySelectorAll<HTMLElement>('[data-pane]')).filter(pane => pane.clientWidth > 0);
    const current = target.closest<HTMLElement>('[data-pane]') ?? panes.find(pane => pane.tagName === 'MAIN');
    const index = current ? panes.indexOf(current) : 1;
    if (event.key === 'h' || event.key === 'l') {
      event.preventDefault(); panes[Math.max(0, Math.min(panes.length - 1, index + (event.key === 'h' ? -1 : 1)))]?.focus();
    }
    if (event.key === 'j' || event.key === 'k') {
      event.preventDefault();
      if (current?.tagName === 'MAIN') current.scrollBy({ top: event.key === 'j' ? 80 : -80 });
      else {
        const links = Array.from(current?.querySelectorAll<HTMLAnchorElement>('a') ?? []);
        const at = links.indexOf(target as HTMLAnchorElement);
        links[Math.max(0, Math.min(links.length - 1, at + (event.key === 'j' ? 1 : -1)))]?.focus();
      }
    }
  }
</script>

<svelte:window onkeydown={keydown} />
<div class="shell" bind:this={shell} style:--type-size={`${scale}px`}>
  <header class="topbar">
    <a class="brand" href="/" aria-label="Loom documentation home"><span class="brand-mark"><Layers size={19} strokeWidth={1.7} /></span><strong>Loom</strong><span class="brand-divider"></span><span class="muted">Documentation</span></a>
    <div class="top-actions"><button class="mobile-toggle" aria-label="Toggle navigation" onclick={() => mobileNav = !mobileNav}><PanelLeft size={16} /></button><a class="search-control" href="/search/"><Search size={14} /><span>Search documentation</span><kbd>⌘ K</kbd></a></div>
  </header>
  <nav class:mobile-open={mobileNav} class="sidebar" aria-label="Documentation" data-pane tabindex="-1">
    <div class="pane-label">WORKSPACE <span>LOOM</span></div>
    {#each nav as section, i}
      {@const Icon = icons[i % icons.length]}
      <section class="nav-group">
        <h2><Icon size={14} color={colors[i % colors.length]} />{section.title}</h2>
        {#each section.items as item}
          <a href={item.href} class:active={page.url.pathname.replace(/\/$/, '') === item.href.replace(/\/$/, '')} aria-current={page.url.pathname.replace(/\/$/, '') === item.href.replace(/\/$/, '') ? 'page' : undefined} onclick={() => mobileNav = false}>{item.title}</a>
        {/each}
      </section>
    {/each}
    <div class="nav-foot">Repository documentation<br /><span>Markdown + generated reference</span></div>
  </nav>
  <main id="main-content" data-pane tabindex="-1">
    <div class="breadcrumb"><span>Docs</span><span>/</span><span>{title}</span></div>
    <div class="page-content">{@render children()}</div>
  </main>
  <aside class="outline" aria-label="On this page" data-pane tabindex="-1">
    <div class="pane-label">ON THIS PAGE</div>
    {#if outline.length}
      <div class="outline-links">{#each outline as heading}<a href={`#${heading.id}`} style:padding-left={`${Math.max(0, heading.level - 2) * 10 + 12}px`}>{heading.text}</a>{/each}</div>
    {:else}<p class="outline-empty">{title === 'Search' ? 'Search headings and paragraphs across the repository.' : 'Generated from repository sources at build time.'}</p>{/if}
    <div class="outline-note"><FileCode2 size={14} color="#c393ae" /><span>Content lives in the repository.</span></div>
  </aside>
  <footer class="statusbar"><span><span class="status-dot"></span>Loom documentation</span><span>Source-driven <span class="status-divider">/</span> <button onclick={() => help = !help}>Keyboard shortcuts <kbd>?</kbd></button></span></footer>
</div>
<dialog bind:this={helpDialog} class="help-panel" aria-label="Keyboard shortcuts" onclose={() => help = false}>
      <div class="help-title"><h2>Keyboard shortcuts</h2><button onclick={() => help = false} aria-label="Close keyboard shortcuts">Close <kbd>Esc</kbd></button></div>
      <dl><dt><kbd>j</kbd> / <kbd>k</kbd></dt><dd>Move down / up in a pane</dd><dt><kbd>h</kbd> / <kbd>l</kbd></dt><dd>Switch panes</dd><dt><kbd>⌘ K</kbd> / <kbd>Ctrl K</kbd></dt><dd>Open search</dd><dt><kbd>+</kbd> / <kbd>−</kbd></dt><dd>Adjust type size</dd><dt><kbd>?</kbd></dt><dd>Show keyboard shortcuts</dd></dl>
</dialog>
