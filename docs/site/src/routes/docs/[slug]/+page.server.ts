import { error } from '@sveltejs/kit';
import { loadDocs } from '$lib/data/docs';
import type { PageServerLoad, EntryGenerator } from './$types';
export const entries: EntryGenerator = async () => (await loadDocs()).map(doc => ({ slug: doc.slug }));
export const load: PageServerLoad = async ({ params }) => {
  const doc = (await loadDocs()).find(doc => doc.slug === params.slug);
  if (!doc) error(404, 'Documentation page not found');
  return { doc };
};
