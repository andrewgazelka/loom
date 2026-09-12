import { loadDocs } from '$lib/data/docs';
export async function load() {
  return { documents: (await loadDocs()).map(doc => ({ slug: doc.slug, title: doc.title, headings: doc.headings, paragraphs: doc.paragraphs })) };
}
