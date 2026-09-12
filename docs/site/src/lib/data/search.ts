import type { DocPage } from './docs';
export type SearchDocument = Pick<DocPage, 'slug' | 'title' | 'headings' | 'paragraphs'>;
export interface SearchResult { documentTitle: string; title: string; excerpt: string; href: string; kind: 'heading' | 'paragraph'; score: number }
export function searchDocuments(documents: SearchDocument[], query: string): SearchResult[] {
  const terms = query.trim().toLocaleLowerCase().split(/\s+/).filter(Boolean);
  if (!terms.length) return [];
  const matches = (text: string) => terms.every(term => text.toLocaleLowerCase().includes(term));
  const results: SearchResult[] = [];
  for (const doc of documents) {
    for (const heading of doc.headings) if (matches(`${doc.title} ${heading.text}`)) results.push({ documentTitle: doc.title, title: heading.text, excerpt: '', href: `/docs/${doc.slug}/#${heading.id}`, kind: 'heading', score: 2 });
    for (const paragraph of doc.paragraphs) if (matches(paragraph)) {
      const first = paragraph.toLocaleLowerCase().indexOf(terms[0]);
      const start = Math.max(0, first - 70);
      results.push({ documentTitle: doc.title, title: doc.title, excerpt: `${start ? '…' : ''}${paragraph.slice(start, start + 250)}${paragraph.length > start + 250 ? '…' : ''}`, href: `/docs/${doc.slug}/`, kind: 'paragraph', score: 1 });
    }
  }
  return results.sort((a, b) => b.score - a.score || a.documentTitle.localeCompare(b.documentTitle));
}
