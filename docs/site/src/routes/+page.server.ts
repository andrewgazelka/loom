import { loadDocs } from '$lib/data/docs';
import { loadCrates } from '$lib/data/crates';
import { loadMcpTools } from '$lib/data/mcp';
export async function load() {
  const docs = await loadDocs();
  return { readme: docs.find(doc => doc.slug === 'readme'), architecture: docs.find(doc => doc.slug === 'architecture'), docCount: docs.length, crateCount: loadCrates().length, toolCount: loadMcpTools().length };
}
