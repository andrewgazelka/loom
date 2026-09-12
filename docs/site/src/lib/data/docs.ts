import { Marked } from 'marked';
import { codeToHtml, bundledLanguages } from 'shiki';
import { parseDoc, type DocPage } from './parsers';
export type { DocPage, Heading } from './parsers';
export { parseDoc } from './parsers';
const sources = import.meta.glob<string>(['../../../../*.md', '../../../../future/*.md', '../../../../../README.md'], { query: '?raw', import: 'default', eager: true });
const escape = (text: string) => text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
let cache: Promise<DocPage[]> | undefined;
export function loadDocs(): Promise<DocPage[]> { return cache ??= buildDocs(); }
async function buildDocs(): Promise<DocPage[]> {
  for (const name of ['architecture.md', 'content-addressed-code.md']) if (!Object.keys(sources).some((path) => path.endsWith(`/${name}`))) console.warn(`[loom-docs] docs/${name}: optional page missing; omitted`);
  const docs = Object.keys(sources).map((path) => { const name = path.split('/').at(-1) ?? ''; if (path.endsWith('/future/README.md')) return parseDoc('docs/future/README.md', sources[path], 'future'); if (path.includes('/future/')) return parseDoc(`docs/future/${name}`, sources[path], `future-${name.replace(/\.md$/i, '').toLowerCase()}`); return parseDoc(path.endsWith('/README.md') ? 'README.md' : `docs/${name}`, sources[path]); });
  const seen = new Set<string>(); for (const doc of docs) { if (seen.has(doc.slug)) throw new Error(`${doc.source}: duplicate slug ${doc.slug}`); seen.add(doc.slug); }
  for (const doc of docs) {
    let headingIndex = 0;
    const renderer = new Marked({ async: true, gfm: true, renderer: {
      heading(token) { const heading = doc.headings[headingIndex++]; return `<h${token.depth} id="${heading.id}">${this.parser.parseInline(token.tokens)}</h${token.depth}>\n`; },
      link(token) {
        let href = token.href;
        if (!/^(?:[a-z]+:|\/|#)/i.test(href)) {
          const target = new URL(href, `https://repository.invalid/${doc.source}`);
          const source = decodeURIComponent(target.pathname.slice(1));
          const page = docs.find((entry) => entry.source === source);
          href = page ? `/docs/${page.slug}${target.hash}` : `https://github.com/andrewgazelka/loom/blob/main/${source}${target.hash}`;
        }
        return `<a href="${escape(href)}">${this.parser.parseInline(token.tokens)}</a>`;
      },
      image(token) {
        let href = token.href;
        if (!/^(?:[a-z]+:|\/|#)/i.test(href)) {
          const target = new URL(href, `https://repository.invalid/${doc.source}`);
          href = `https://raw.githubusercontent.com/andrewgazelka/loom/main${target.pathname}`;
        }
        return `<img src="${escape(href)}" alt="${escape(token.text)}"${token.title ? ` title="${escape(token.title)}"` : ''} loading="lazy" />`;
      }
    }, walkTokens: async (token) => {
      if (token.type !== 'code') return;
      const language = token.lang?.split(/\s/)[0] ?? 'text';
      const html = language === 'mermaid' ? `<pre class="mermaid">${escape(token.text)}</pre>` : await codeToHtml(token.text, { lang: language in bundledLanguages ? language as keyof typeof bundledLanguages : 'text', themes: { light: 'github-light', dark: 'github-dark' } });
      Object.assign(token, { type: 'html', text: html, block: true });
    } });
    try { doc.html = await renderer.parse(doc.markdown); } catch (error) { throw new Error(`${doc.source}: rendering failed: ${String(error)}`); }
  }
  return docs.sort((a, b) => a.title.localeCompare(b.title));
}
