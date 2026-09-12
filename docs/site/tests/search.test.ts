import { expect, test } from 'bun:test';
import { parseDoc } from '../src/lib/data/parsers';
import { searchDocuments } from '../src/lib/data/search';

test('search preserves identifiers and links headings to their actual anchors', () => {
  const doc = parseDoc('docs/tools.md', '# Tools\n\n## `loom_define`\n\nCall `loom_define` with **Rust source**.');
  const results = searchDocuments([doc], 'loom_define');
  expect(results).toHaveLength(2);
  expect(results[0].href).toBe('/docs/tools/#loom_define');
  expect(results[1].excerpt).toBe('Call loom_define with Rust source.');
  expect(searchDocuments([doc], 'RUST source')).toHaveLength(1);
  expect(searchDocuments([doc], 'unknown')).toHaveLength(0);
  expect(searchDocuments([doc], ' ')).toHaveLength(0);
});
