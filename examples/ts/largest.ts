import { fs, type Value } from "loom";
// fs.list returns typed entries encoded as [name, size, kind] tuples;
// kind is "file", "dir", "symlink" or "other".
type Entry = [string, number, string];
export default function largest(machine: Value, path: string): { name: string; size: number } | null {
  const entries = fs.list({ machine, path }) as unknown as Entry[];
  let largest: { name: string; size: number } | null = null;
  for (const [name, size, kind] of entries) {
    if (kind === "file" && (largest === null || size > largest.size)) largest = { name, size };
  }
  return largest;
}
