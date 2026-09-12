import { loadDocs } from '$lib/data/docs';
import { getNav } from '$lib/data/nav';
export async function load() {
  const docs = await loadDocs();
  return { nav: getNav(docs) };
}
