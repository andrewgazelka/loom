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
  **stable** toolchain in `toolchain.nix` (`toolchains.host`). cargoUnit
  otherwise passes `-Zembed-metadata=no`, which a stable rustc rejects
  outright; its own guard reads a channel tag that only index-built toolchains
  carry, so it cannot see this one.
- The quality gates (clippy, tests, cargo-audit, cargo-machete, unused-crate
  denial) are off. They belong to this repo's own checks; this workspace
  exists to produce two binaries.

`flake.nix` also makes `index` follow a current `rust-overlay`: the revision
`index` pins reads `stdenv.isLinux`, and that deprecation warning aborts
evaluation wherever `abort-on-warn` is set. Drop the `follows` once `index`
pins a rust-overlay that reads `stdenv.hostPlatform.isLinux`.

## Two toolchains, one in the runtime closure

`toolchain.nix` reads `rust-toolchain-manifest.json` and assembles both pinned
toolchains from official signed archives (`update-rust-toolchain.py` refreshes
the pins; it verifies Rust's detached signature over each release manifest
before it writes a hash).

| entry | channel | what it is |
| --- | --- | --- |
| `host` | `1.97.0` | builds `loomd` and `loom`; build-time only |
| `guest` | `nightly-2026-08-24` | what the daemon compiles guest definitions with, and what `tools/hash-rustc` is built against |

Only `guest` is on the daemon's `PATH` and in the package's runtime closure.
It carries `rustc`, `cargo`, the host and `wasm32-unknown-unknown` standard
libraries, `rust-src`, and two components the driver needs: `rustc-dev` (the
compiler's own crates, which a rustc plugin links against) and
`llvm-tools-preview` (the `llvm-objcopy` the driver's object cache runs).

The pin is `tools/hash-rustc/rust-toolchain.toml`'s channel, and it has to be:
a rustc plugin links against the exact compiler that runs it, and `loomd`
refuses a driver whose `hash-rustc -vV` differs from `$RUSTC -vV`.

## The prebuilt driver: `hash-rustc.nix`

`loom.sh` exports `LOOM_HASH_RUSTC=<store path>/bin/hash-rustc` and
`RUSTC=<guest toolchain>/bin/rustc`. Nothing is compiled at run time: the
driver's build script bakes the guest toolchain's sysroot into the binary and
links it with `-Wl,-rpath,<sysroot>/lib`, so `hash-rustc -vV` answers from the
store with no toolchain on `PATH`.

It is built with `rustPlatform.buildRustPackage`, not `cargoUnit`, and its
cargo is **nixpkgs'**, not the pinned toolchain's. The reason is local
buildability on macOS: the official `aarch64-apple-darwin` cargo from
static.rust-lang.org links `/usr/lib/libcurl`, whose LibreSSL reads
`/private/etc/ssl/openssl.cnf` at startup. A Nix build sandbox denies that
path, LibreSSL reports `Auto configuration failed`, and the process exits 1
before cargo does anything; `sandbox-paths` does not cover `/private/etc`, and
`allowed-impure-host-deps` cannot be widened from inside a derivation. Every
cargoUnit stage runs `rustToolchain`'s own cargo, so cargoUnit would need a
toolchain whose cargo comes from nixpkgs. `rustc` is unaffected, and rustc is
what decides compiler identity, so the driver is still built by, and pinned
to, the guest nightly. (Reproduce: run the pinned toolchain's `cargo
--version` inside any `runCommand`.)

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
| `toolchain` | the pinned guest compiler: `nightly-2026-08-24` (`toolchain.nix`) |
| `buildToolchain` | the pinned compiler that builds `loomd` and `loom`: `1.97.0` |
| `driver` | `bin/hash-rustc`, prebuilt against `toolchain` (`hash-rustc.nix`) |
| `sources` | the runtime source tree the daemon compiles guests against |

`javascript.nix` builds the Svelte app that `sources` links in as `ui/build`.
