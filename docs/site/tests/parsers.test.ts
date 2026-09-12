import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { parseDoc, parseCrate, parseMcpTools } from '../src/lib/data/parsers';
const fixture = (name: string) => readFileSync(new URL(`fixtures/${name}`, import.meta.url), 'utf8');
describe('docs parser', () => {
  test('front matter, unique headings, fenced code and paragraphs', () => {
    const page = parseDoc('docs/example.md', fixture('page.md'));
    expect(page.slug).toBe('sample'); expect(page.title).toBe('Example page');
    expect(page.headings.map((heading) => heading.id)).toEqual(['original-title', 'repeat', 'repeat-1']);
    expect(page.paragraphs).toEqual(['A paragraph with bold text.']); expect(page.wordCount).toBeGreaterThan(10);
  });
  test('derives title and README slug', () => { expect(parseDoc('README.md', '# Loom\n\nHello').slug).toBe('readme'); });
  test('names malformed file', () => {
    for (const text of ['---\ntitle: broken', '---\ntitle: 4\n---\n# Hi', 'No heading', '---\nslug: ../bad\n---\n# Hi']) expect(() => parseDoc('bad.md', text)).toThrow('bad.md:');
  });
});
describe('crate parser', () => {
  test('extracts manifest and Rust crate docs', () => { expect(parseCrate('crates/example/Cargo.toml', fixture('Cargo.toml'), '//! First documentation.\n//! Second.')).toEqual({ name: 'loom-example', description: 'An example crate', summary: 'First documentation.', path: 'crates/example' }); });
  test('allows omitted description', () => { expect(parseCrate('Cargo.toml', '[package]\nname="small"').description).toBe(''); });
  test('names invalid TOML and missing package', () => { for (const text of ['[broken', '[package]\nname=42', '[workspace]']) expect(() => parseCrate('broken/Cargo.toml', text)).toThrow('broken/Cargo.toml:'); });
});
describe('MCP parser', () => {
  test('only tools inside MCP surface', () => { expect(parseMcpTools('actors.md', fixture('mcp.md'))).toEqual([{ name: 'actor_spawn', description: 'Spawn an actor.' }, { name: 'actor_send', description: 'Send a message.' }]); });
  test('bullet surface', () => { expect(parseMcpTools('actors.md', '## MCP surface\n- `actor_spawn` — Spawn actor.')[0].name).toBe('actor_spawn'); });
  test('rejects absent, empty and duplicate tool sections with filename', () => { for (const text of ['# Actors', '## MCP surface\nNo tools', '## MCP surface\n- `spawn` — Spawn\n- `spawn` — Again']) expect(() => parseMcpTools('bad-actors.md', text)).toThrow('bad-actors.md:'); });
});

test('heading anchors remain unique when generated suffixes collide with literal headings', () => {
  const page = parseDoc('collision.md', '# Repeat\n## Repeat\n## Repeat-1\n## Repeat\n## !!!\n## ???');
  expect(page.headings.map((heading) => heading.id)).toEqual(['repeat', 'repeat-1', 'repeat-1-1', 'repeat-2', 'section', 'section-1']);
});

test('MCP rejects malformed rows even alongside a valid tool', () => {
  for (const broken of ['| `broken` |', '| `bad name` | Description |', '- `broken`', '- `broken` —']) {
    expect(() => parseMcpTools('mixed.md', `## MCP surface\n| Tool | Description |\n| --- | --- |\n| good | Works |\n${broken}`)).toThrow('mixed.md:');
  }
});
