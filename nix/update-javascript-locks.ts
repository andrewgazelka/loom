// Regenerate after changing any Bun lock: bun nix/update-javascript-locks.ts.
// Integrity is supplied by npm in Bun's lock; fetchurl verifies the archive bytes.
import { join } from 'node:path';
const root = join(import.meta.dir, '..');
const result: Record<string, unknown> = {};
for (const directory of ['ui']) {
  const text = await Bun.file(join(root, directory, 'bun.lock')).text();
  const lock = Bun.JSONC.parse(text);
  if (lock.lockfileVersion !== 1) throw new Error(`Unsupported Bun lock in ${directory}`);
  const packages = Object.entries(lock.packages).map(([placement, entry]) => {
    const fields = entry as unknown[];
    const identity = fields[0] as string;
    const registry = fields[1] as string;
    const metadata = fields[2] as Record<string, unknown>;
    const integrity = fields[3] as string;
    const separator = identity.lastIndexOf('@');
    const name = identity.slice(0, separator);
    const version = identity.slice(separator + 1);
    if (separator <= 0 || registry !== '' || !/^sha512-[A-Za-z0-9+/]+=*$/.test(integrity)) {
      throw new Error(`Unsupported registry dependency ${placement} in ${directory}`);
    }
    // Bun lock keys describe hoisted placement. A scope and package form one segment.
    const segments = placement.match(/@[^/]+\/[^/]+|[^/]+/g)!;
    const path = `node_modules/${segments.join('/node_modules/')}`;
    const installedName = segments.at(-1)!;
    const binDirectory = path.slice(0, -(installedName.length + 1)) + '/.bin';
    const basename = name.split('/').at(-1)!;
    return {
      name, version, path, installedName, binDirectory,
      url: `https://registry.npmjs.org/${name}/-/${basename}-${version}.tgz`,
      hash: integrity,
      os: metadata.os ?? [], cpu: metadata.cpu ?? [],
      bins: typeof metadata.bin === 'string'
        ? { [basename]: metadata.bin }
        : (metadata.bin ?? {}) as Record<string, string>,
    };
  });
  // npm aliases can expose a bin already owned by the unaliased package.
  // Preserve the canonical owner, rather than making link order choose a version.
  const binOwners = new Map<string, (typeof packages)[number]>();
  for (const pkg of packages) {
    for (const bin of Object.keys(pkg.bins)) {
      const key = `${pkg.binDirectory}/${bin}`;
      const previous = binOwners.get(key);
      if (!previous) { binOwners.set(key, pkg); continue; }
      const canonical = pkg.installedName === pkg.name;
      const previousCanonical = previous.installedName === previous.name;
      if (canonical === previousCanonical) throw new Error(`Ambiguous bin ${key}`);
      const winner = canonical ? pkg : previous;
      const loser = canonical ? previous : pkg;
      delete loser.bins[bin];
      binOwners.set(key, winner);
    }
  }
  result[directory] = {
    lockHash: new Bun.CryptoHasher('sha256').update(text).digest('hex'), packages,
  };
}
await Bun.write(join(import.meta.dir, 'javascript-locks.json'), JSON.stringify(result, null, 2) + '\n');
