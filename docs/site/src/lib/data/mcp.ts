import { parseMcpTools, type McpTool } from './parsers';
export type { McpTool } from './parsers';
export { parseMcpTools } from './parsers';
const sources = import.meta.glob<string>('../../../../actors-turso.md', { query: '?raw', import: 'default', eager: true });
export function loadMcpTools(): McpTool[] {
  const source = 'docs/actors-turso.md'; const raw = Object.values(sources)[0];
  if (raw === undefined) throw new Error(`${source}: missing required actor specification`);
  if (!/^#{1,6}\s+MCP surface\b/im.test(raw)) { console.warn(`[loom-docs] ${source}: MCP surface section absent; tool table omitted`); return []; }
  return parseMcpTools(source, raw);
}
