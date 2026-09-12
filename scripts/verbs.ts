/** Read the Rust verb table used by every transport. */
export async function commandNames(): Promise<string[]> {
  const path = new URL('../crates/loom-proto/src/verbs.rs', import.meta.url);
  const source = await Bun.file(path).text();
  const table = source.match(/pub static VERBS: &[\s\S]*?= &\[([\s\S]*?)\n\];/);
  if (!table) throw new Error(`Missing VERBS table in ${path.pathname}`);
  const names = [...table[1]!.matchAll(/verb!\s*\(\s*([a-z_]+)\s*,/g)].map(match => match[1]!);
  if (!names.length || new Set(names).size !== names.length) {
    throw new Error(`Invalid verb names in ${path.pathname}`);
  }
  return names.sort();
}
