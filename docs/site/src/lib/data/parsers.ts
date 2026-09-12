import { marked, type Token } from 'marked';
import { parse as parseYaml } from 'yaml';
import { parse as parseToml, type TomlTable } from 'smol-toml';

export interface Heading { id: string; text: string; level: number }
export interface DocPage { slug: string; title: string; description: string; source: string; markdown: string; html: string; headings: Heading[]; wordCount: number; paragraphs: string[] }
export interface CrateInfo { name: string; description: string; summary: string; path: string }
export interface McpTool { name: string; description: string }
export function plainText(text: string): string {
  function readTokens(tokens: Token[]): string {
    return tokens.map((token) => {
      if (token.type === 'html') return '';
      if ('tokens' in token && Array.isArray(token.tokens)) return readTokens(token.tokens);
      return 'text' in token && typeof token.text === 'string' ? token.text : ' ';
    }).join('');
  }
  return readTokens(marked.Lexer.lexInline(text)).replace(/\s+/g, ' ').trim();
}
export function headingId(text: string): string { return plainText(text).toLowerCase().replace(/[^\p{L}\p{N}_\s-]/gu, '').replace(/\s+/g, '-').replace(/-+/g, '-'); }
function fail(source: string, message: string): never { throw new Error(`${source}: ${message}`); }
export function parseDoc(source: string, raw: string): DocPage {
  let markdown = raw.replace(/^\uFEFF/, '');
  let metadata: Record<string, unknown> = {};
  if (markdown.startsWith('---\n') || markdown.startsWith('---\r\n')) {
    const front = markdown.match(/^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/);
    if (!front) fail(source, 'unterminated front matter');
    try { const value: unknown = parseYaml(front[1]); if (!value || typeof value !== 'object' || Array.isArray(value)) fail(source, 'front matter must be a mapping'); metadata = value as Record<string, unknown>; } catch (error) { fail(source, `invalid front matter: ${String(error)}`); }
    markdown = markdown.slice(front[0].length);
  }
  for (const key of ['title', 'slug', 'description']) if (metadata[key] !== undefined && typeof metadata[key] !== 'string') fail(source, `front matter ${key} must be a string`);
  const headings: Heading[] = []; const paragraphs: string[] = []; const used = new Set<string>();
  const tokens = marked.lexer(markdown);
  marked.walkTokens(tokens, (token: Token) => {
    if (token.type === 'heading') {
      const base = headingId(token.text) || 'section';
      let id = base; let suffix = 1;
      while (used.has(id)) id = `${base}-${suffix++}`;
      used.add(id);
      headings.push({ id, text: plainText(token.text), level: token.depth });
    }
    if (token.type === 'paragraph') paragraphs.push(plainText(token.text));
  });
  const title = (metadata.title as string | undefined) ?? headings.find((heading) => heading.level === 1)?.text;
  if (!title?.trim()) fail(source, 'page needs a title in front matter or an H1 heading');
  const filename = source.split('/').at(-1) ?? '';
  const slug = (metadata.slug as string | undefined) ?? filename.replace(/\.md$/i, '').toLowerCase();
  if (!/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(slug)) fail(source, `invalid slug ${JSON.stringify(slug)}`);
  return { slug, title, description: (metadata.description as string | undefined) ?? paragraphs[0] ?? '', source, markdown, html: '', headings, paragraphs, wordCount: plainText(markdown).split(/\s+/).filter(Boolean).length };
}
export function parseCrate(source: string, raw: string, rustSource = ''): CrateInfo {
  let value: TomlTable;
  try { value = parseToml(raw); } catch (error) { fail(source, `invalid TOML: ${String(error)}`); }
  const pkg = value.package;
  if (!pkg || typeof pkg !== 'object' || Array.isArray(pkg) || pkg instanceof Date) fail(source, 'missing [package] table');
  const fields = pkg as Record<string, unknown>;
  if (typeof fields.name !== 'string' || !fields.name.trim()) fail(source, 'package.name must be a nonempty string');
  if (fields.description !== undefined && typeof fields.description !== 'string') fail(source, 'package.description must be a string');
  return { name: fields.name, description: fields.description as string | undefined ?? '', summary: rustSource.match(/^\s*\/\/(?:!|\/)\s*(.+)$/m)?.[1]?.trim() ?? '', path: source.replace(/\/Cargo\.toml$/, '') };
}
export function parseMcpTools(source: string, raw: string): McpTool[] {
  const lines = raw.split(/\r?\n/); const start = lines.findIndex((line) => /^#{1,6}\s+MCP surface\b/i.test(line));
  if (start < 0) fail(source, 'missing MCP surface section');
  const depth = lines[start].match(/^#+/)![0].length; const section: string[] = [];
  for (const line of lines.slice(start + 1)) { const heading = line.match(/^(#+)\s/); if (heading && heading[1].length <= depth) break; section.push(line); }
  const tools: McpTool[] = []; const names = new Set<string>();
  for (const line of section) {
    const table = line.match(/^\s*\|\s*`?([a-z][a-z0-9_.-]+)`?\s*\|\s*(.*?)\s*\|\s*$/i);
    const bullet = line.match(/^\s*[-*]\s+`([a-z][a-z0-9_.-]+)`\s*(?:[:—–-]\s*)?(.+)$/i);
    const match = table ?? bullet;
    if (!match) {
      const separator = /^\s*\|(?:\s*:?-{3,}:?\s*\|)+\s*$/.test(line);
      if (!separator && (/^\s*\|/.test(line) || /^\s*[-*]\s+`/.test(line))) fail(source, `malformed MCP tool row: ${line.trim()}`);
      continue;
    }
    if (/^(tool|name)$/i.test(match[1])) continue;
    if (!plainText(match[2]).replace(/^[:—–-]$/, '').trim()) fail(source, `MCP tool ${match[1]} needs a description`);
    if (names.has(match[1])) fail(source, `duplicate MCP tool ${match[1]}`);
    names.add(match[1]); tools.push({ name: match[1], description: plainText(match[2]) });
  }
  if (!tools.length) fail(source, 'MCP surface has no tool rows');
  return tools;
}
