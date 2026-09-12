import type { DocPage } from './parsers';

export interface NavItem { title: string; href: string }
export interface NavSection { title: string; items: NavItem[] }

const labels: Record<string, string> = {
  readme: 'Repository README',
  guide: 'Setup and API guide',
  architecture: 'System architecture',
  'plan-shared-execution': 'Shared execution',
  'shared-core-abi': 'Shared-core ABI',
  'actors-turso': 'Actor specification',
  'plan-effects': 'Guest-defined effects',
  'content-addressed-handlers': 'Stored handlers',
  'content-addressed-code': 'Content-addressed code',
  'plan-unified-memory': 'Memory design history',
  future: 'Future work: index'
};

export function getNav(docs: DocPage[]): NavSection[] {
  const assigned = new Set<string>();
  function pages(slugs: string[]): NavItem[] {
    return slugs.flatMap((slug) => {
      const doc = docs.find((page) => page.slug === slug);
      if (!doc) return [];
      assigned.add(slug);
      return [{ title: labels[slug] ?? doc.title, href: `/docs/${slug}/` }];
    });
  }
  const sections: NavSection[] = [
    { title: 'Start', items: [{ title: 'Overview', href: '/' }, ...pages(['readme', 'index', 'guide'])] },
    { title: 'Architecture', items: pages(['architecture', 'plan-shared-execution', 'shared-core-abi', 'plan-unified-memory']) },
    { title: 'Actors', items: pages(['actors-turso']) },
    { title: 'Effects and handlers', items: pages(['plan-effects', 'content-addressed-handlers']) },
    { title: 'Content-addressed code', items: pages(['content-addressed-code']) },
    { title: 'Future work', items: [...pages(['future']), ...docs.filter((doc) => doc.slug.startsWith('future-')).sort((a, b) => a.title.localeCompare(b.title)).map((doc) => { assigned.add(doc.slug); return { title: doc.title, href: `/docs/${doc.slug}/` }; })] },
    { title: 'MCP', items: [{ title: 'Tool reference', href: '/mcp/' }] },
    { title: 'Crates', items: [{ title: 'Crate catalog', href: '/crates/' }] },
    { title: 'Reference', items: [{ title: 'Search documentation', href: '/search/' }, ...docs.filter((doc) => !assigned.has(doc.slug)).map((doc) => ({ title: doc.title, href: `/docs/${doc.slug}/` }))] }
  ];
  return sections.filter((section) => section.items.length);
}
