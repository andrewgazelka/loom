# nix/

Everything `nix run`, `nix shell`, and `nix build` reach in this repo.

## Entry point

```sh
nix run .#repl               # loomd, then the browser REPL at the printed URL
nix run .                    # loomd alone (add `-- --stdio` for MCP over stdio)
nix shell . -c loom --help   # the loom CLI on PATH
nix build .                  # result/bin/loomd, result/bin/loom
```

`nix run .#repl` (`repl.sh`) starts `loomd`, waits for the bind address to
answer and for the daemon to survive that, then prints two lines and opens the
first in a browser:

```
Loom REPL: http://127.0.0.1:8787/#token=<token>
CLI: <store path of the loom-cli binary>/bin/loom
```

A connection to the bind address proves something answers there, not that it
is ours, so the daemon's own liveness is the verdict: when the address is
already taken, `loomd` exits, and the script reports its status and exits with
it rather than printing a URL that points at whatever else holds the port.

The browser opens through `open` on Darwin (no display check) and through
`xdg-open` on Linux, there gated on `DISPLAY` or `WAYLAND_DISPLAY`; with no
opener it says so on stderr and keeps the daemon running. The token rides the
URL **fragment**, never a query string, so it never reaches the server or its
access logs. The token comes from `--token`, else `LOOM_TOKEN`, else the
generated `$state/token` file (waited for, like the bind address).
`--tokens-file` names a file of several tokens rather than one bearer secret,
so that case prints a plain `http://<bind>/` and you supply a token yourself.
`nix run .#repl -- --help` prints the daemon's own `--help` and starts nothing;
like the launcher, that is a check on the first argument only.

## Two programs, two names

`loomd` is the daemon: `loom.sh` arranges its state directory, token file,
database, actors directory, guest toolchain, and certificate bundle, then
`exec`s the `loomd` binary. It is `meta.mainProgram`, so `nix run .` starts
it. `loom` is the CLI binary from `crates/loom-cli` (its Cargo target is
named `loom`), symlinked into the same `bin/`. Nothing is named twice.

## Host build: `ix.cargoUnit`

`packages.nix` builds `loomd` and `loom` with `cargoUnit.buildWorkspace` from
the `index` flake input: it transcribes Cargo's unit graph into one Nix
derivation per rustc invocation, so editing one crate rebuilds that crate and
its dependents rather than the workspace. Crates.io dependencies are vendored
from `Cargo.lock`'s checksums; there is no generated-Nix file to regenerate
and no second build path.

Two things this workspace has to tell it:

- `policy.compiler.embedMetadata = true`. The compiler here is the pinned
  **stable** toolchain in `toolchain.nix` (the same one the daemon ships to
  guests). cargoUnit otherwise passes `-Zembed-metadata=no`, which a stable
  rustc rejects outright; its own guard reads a channel tag that only
  index-built toolchains carry, so it cannot see this one.
- The quality gates (clippy, tests, cargo-audit, cargo-machete, unused-crate
  denial) are off. They belong to this repo's own checks; this workspace
  exists to produce two binaries.

`flake.nix` also makes `index` follow a current `rust-overlay`: the revision
`index` pins reads `stdenv.isLinux`, and that deprecation warning aborts
evaluation wherever `abort-on-warn` is set. Drop the `follows` once `index`
pins a rust-overlay that reads `stdenv.hostPlatform.isLinux`.

`crates/loom-rt` has no build script. Its compilation-cache namespace is
computed at engine construction from `Engine::precompile_compatibility_hash`
(see `docs/content-addressed-code.md`), so no unit needs `cargo metadata` or
the workspace root at build time.

## Attributes

| attribute | what |
| --- | --- |
| `default` | `bin/loomd` (launcher) + `bin/loom` (CLI) + `libexec/bun` |
| `repl` | `bin/loom-repl`, the launcher `nix run .#repl` runs |
| `daemon`, `cli` | the two rustc-built binaries, unwrapped |
| `host` | both binaries joined under one prefix |
| `toolchain` | the pinned Rust compiler plus `rust-src` (`toolchain.nix`) |
| `sources` | the runtime source tree the daemon compiles guests against |

`javascript.nix` builds the Svelte app that `sources` links in as `ui/build`.
