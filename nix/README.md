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

### V8 native inputs

`v8.nix` supplies V8's native archive and matching generated Rust bindings as
fixed-output inputs to the V8 Cargo unit. To update, change the exact dependency
in `crates/loom-v8/Cargo.toml`, then run `python3 nix/update-v8.py` and review
`v8-manifest.json`. The updater downloads and hashes both supported platforms;
upstream publishes no signed release manifest.

These are the official pointer-compressed release archives. V8 152.2.0 does
not publish the additional memory-corruption sandbox variant. JavaScript's host
API isolation remains Loom's responsibility; these archives do not enable V8's
memory-corruption cage.

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
cargo is **nixpkgs'**, not the pinned toolchain's. Through cargoUnit this
crate's `cargo-unit-graph.json` stage fails on aarch64-darwin, with both
pinned toolchains, and the build log is only this:

```
Auto configuration failed
...:Operation not permitted:...fopen('/private/etc/ssl/openssl.cnf', 'rb')
```

That is Apple's LibreSSL, reached through the `/usr/lib/libcurl` the official
static.rust-lang.org cargo links, aborting because the build sandbox denies
`/private/etc` (`sandbox-paths` does not cover it, and a derivation cannot
widen `allowed-impure-host-deps`). The same `cargo build --unit-graph`
command succeeds outside the sandbox, and the host workspace's own graph
stage, same cargo, builds fine; what this crate does differently to reach
curl at all is unresolved, and the open question is
[docs/future/cargo-unit-hash-rustc.md](../docs/future/cargo-unit-hash-rustc.md).

nixpkgs' cargo links nixpkgs' curl and openssl and is unaffected. Cargo is
only the build driver here: `rustc` decides compiler identity, and rustc is
the pinned nightly either way.

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

## Darwin links guest build scripts with Apple's cc

`nix/loom.sh` exports `CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/cc` on
macOS. rustc places its sysroot's `lib` on the linker's dyld search path, and
nixpkgs' clang loads `libLLVM.dylib` by name, so with the nightly guest
toolchain on that path the packaged `cc` aborted with `Symbol not found:
_LLVMInitializeLanaiAsmParser` while linking a dependency's build script
(the fifth run of `scripts/e2e-unison.sh`). Apple's `cc` links against no LLVM
dylib. The launcher refuses to start without `/usr/bin/cc`. The wasm32 guest
itself links with rust-lld and is unaffected; Linux keeps the nixpkgs linker.

## JavaScript import compiler

`deno.nix` pins the official Deno CLI and its exact native esbuild helper for
Darwin ARM64 and Linux x86_64. Update both with
`python3 nix/update-deno.py 2.9.6`; the updater checks Deno's upstream helper
version and cache layout, downloads each archive, and generates real hashes.
Deno's native bundler does not honor `ESBUILD_BINARY_PATH`, so the wrapper seeds
its versioned helper cache atomically before invocation.

The `loom-imports` Cargo unit embeds this wrapper as `LOOM_DENO`. Each admission
provides an isolated `LOOM_IMPORT_ROOT` and child `DENO_DIR`. Linux uses bubblewrap
to expose only that directory, the exact compiler runtime closure and DNS files.
Darwin uses a deny-by-default Seatbelt profile. Bundling retains network access
for admission-time imports; guest isolates execute the stored bundle without
module fetching. Tool installation and package scripts are separate from module
resolution: the native helper is pinned and lifecycle scripts are disabled.

## Linux virtual machines

Linux builds embed `LOOM_VM_RUNNER`, `LOOM_VM_LIBRARY`, `LOOM_VM_BWRAP` and
`LOOM_VM_RUNTIME_ROOTS_FILE` in the daemon. The runner has a separate Cargo target
graph so supplying its path to the daemon cannot create a build dependency
cycle. `pkgs.libkrun` comes from the existing nixpkgs lock and includes its Linux
kernel; guests supply a filesystem image rather than another kernel.

Nix generates the exact transitive runner and libkrun closure as a newline-delimited
path file. The host sandbox binds those package paths read-only, exposes `/dev/kvm`,
disables host networking, and copies the
admitted image into a private, size-limited `/guest` tmpfs. Native verification
covered Linux boot, Unicode stdin/stdout, separate stderr, exit status, and a
guest write failing with `ENOSPC` at the tmpfs limit. No macOS VM backend is
configured by this package.

`libkrun.nix` applies two local fixes to the nixpkgs-pinned release: exact JSON
launch configuration (Unicode, arguments, environment and working directory)
and accounting for the embedded kernel inside the requested RAM. Its build runs
five parser controls with ASan/UBSan and two focused memory-layout regressions.
When updating the nixpkgs lock, rebase these patches against that release and run
the package checks plus the native VM gate before removing or changing either
patch. Version and source hashes remain owned by nixpkgs.
