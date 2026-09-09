import { fs, type Value } from "loom";
interface Entry { name: string; size: number; is_dir: boolean }
export default function largest(machine: Value, path: string): Entry | null {
  const entries = fs.list({ machine, path }) as unknown as Entry[];
  let largest: Entry | null = null;
  for (const entry of entries) {
    if (!entry.is_dir && (largest === null || entry.size > largest.size)) largest = entry;
  }
  return largest;
}
