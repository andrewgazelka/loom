// Regenerate after changing any Bun lock: bun nix/update-javascript-locks.ts.
// Integrity is supplied by npm in Bun's lock; fetchurl verifies the archive bytes.
import { join } from 'node:path';
const root = join(import.meta.dir, '..');
const result: Record<string, unknown> = {};
for (const directory of ['loom-checker', 'loom-guest-ts', 'loom-ui']) {
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
    const basename = name.split('/').at(-1)!;
    return {
      name, version, path,
      url: `https://registry.npmjs.org/${name}/-/${basename}-${version}.tgz`,
      hash: integrity,
      os: metadata.os ?? [], cpu: metadata.cpu ?? [],
      bins: metadata.bin ?? {},
    };
  });
  result[directory] = {
    lockHash: new Bun.CryptoHasher('sha256').update(text).digest('hex'), packages,
  };
}
await Bun.write(join(import.meta.dir, 'javascript-locks.json'), JSON.stringify(result, null, 2) + '\n');
