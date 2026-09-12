import { loadMcpTools } from '$lib/data/mcp';
export function load() { return { tools: loadMcpTools() }; }
