import { parseCrate, type CrateInfo } from './parsers';
export type { CrateInfo } from './parsers';
export { parseCrate } from './parsers';
const manifests = import.meta.glob<string>('../../../../../crates/*/Cargo.toml', { query: '?raw', import: 'default', eager: true });
const rust = import.meta.glob<string>('../../../../../crates/*/src/{lib,main}.rs', { query: '?raw', import: 'default', eager: true });
export function loadCrates(): CrateInfo[] {
  return Object.keys(manifests).map((path) => {
    const base = path.replace(/Cargo.toml$/, '');
    return parseCrate(path.replace('../../../../../', ''), manifests[path], rust[`${base}src/lib.rs`] ?? rust[`${base}src/main.rs`] ?? '');
  }).sort((a, b) => a.name.localeCompare(b.name));
}
