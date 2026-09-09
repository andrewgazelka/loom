# Loom

Loom runs TypeScript and Rust WebAssembly components behind one Rust actor host. Definitions, component bytes, event payloads, and effect results are content-addressed with BLAKE3. SQLite WAL holds the append-only event history and rebuildable indexes.

## Run locally

On Apple Silicon macOS or x86-64 Linux, Nix supplies the daemon, Svelte app, checker, and both guest toolchains:

```sh
nix run .
```

Open <http://127.0.0.1:8787> and enter the token from the file printed by the launcher. The database, token, and build caches persist under `~/Library/Application Support/loom` on macOS or `~/.local/share/loom` on Linux. `XDG_DATA_HOME` changes the base directory; `LOOM_DATA_DIR` sets the complete directory. The first Nix build downloads and compiles dependencies.

Pass daemon options after `--`, for example `nix run . -- --bind 127.0.0.1:8788` or `nix run . -- --stdio` for an MCP client. Set `LOOM_TOKEN` to choose a token instead of generating one. Linux process isolation uses the packaged Bubblewrap; machine execution remains platform-dependent.

### Development without Nix

Install Rust 1.97 or newer, Bun 1.3.13, `cargo-component` 0.21.1, and the `wasm32-wasip1` and `wasm32-wasip2` targets. Linux machine execution also needs Bubblewrap. The component builder uses the StarlingMonkey engine shipped in the locked `@bytecodealliance/componentize-js` package.

```sh
rustup target add wasm32-wasip1 wasm32-wasip2
cargo install --locked cargo-component --version 0.21.1
(cd loom-checker && bun install --frozen-lockfile)
(cd loom-guest-ts && bun install --frozen-lockfile)
(cd loom-ui && bun install --frozen-lockfile && bun run build)
export LOOM_TOKEN='replace-with-your-token'
cargo run --release -p loomd -- --db loom.sqlite
```

Open <http://127.0.0.1:8787> and enter the same token. The daemon serves the built Svelte application, HTTP API, WebSocket event stream, and MCP endpoint. The token authorizes the single owner; keep the listener on loopback unless network access is intended.

```sh
cargo run -p loom-cli -- --eval '6 * 7'
cargo run -p loomd -- --db loom.sqlite --stdio
```

The second command starts the MCP stdio transport. Configure an MCP client to run it with `LOOM_TOKEN` and an absolute `--root` pointing at this checkout. Streamable HTTP MCP is at `/mcp` and requires the same bearer token.

## Define and call

```sh
curl -sS http://127.0.0.1:8787/v1/define \
  -H "Authorization: Bearer $LOOM_TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"add","source":"export function main(a: number, b: number): number { return a + b; }"}'
```

The returned definition hash identifies the callable. Invoke `POST /v1/command` with `{"command":"call","args":{"hash":"<hash>","args":[20,22]}}`. TS definitions build on first use; Rust definitions build during `define` and return structured cargo diagnostics. A Rust single-file definition uses the same endpoint with `"lang":"rust"` and source such as:

```rust
#[loom::def]
pub fn add(a: i64, b: i64) -> i64 { a + b }
```

Actors export `run` and `fold` in TS or implement `loom::Actor` under `#[loom::actor]` in Rust. `examples/` contains executable fixtures. Actor commands include `spawn`, `send`, `state`, `fork`, and `upgrade`. Cross-language calls use the same DAG-CBOR values and host effect dispatcher.

Client responses contain `ok`, `seq`, `result`, and `diagnostics`. Results above 8 KB become CAS references. Use the `resolve` command to retrieve a reference. WebSocket clients connect to `/v1/stream` and send `{ "token": "...", "after": 0 }` as their first message; the server streams durable events after that cursor.

## DAG-CBOR and links

The Rust and TS guests use deterministic DAG-CBOR at the WIT boundary. Structured CAS values use the same codec; component binaries, source bundles, and other raw bytes retain the raw codec. JSON clients represent a link as exactly `{ "$ref": "<CID>" }`. In DAG-CBOR this becomes tag 42 containing the zero-prefixed binary CID. Local links use CIDv1 with a BLAKE3-256 digest and distinguish DAG-CBOR (`0x71`) from raw bytes (`0x55`). Definition identities remain source hashes.

Maps have string keys ordered by encoded length and then bytes. Decoders reject duplicate keys, nonminimal or indefinite encodings, other tags, malformed CIDs, undefined, nonfinite floats, and trailing bytes. Floats use 64 bits. The shared JSON value model encodes safe integral numbers as integers; Rust integers outside JavaScript's safe range are rejected instead of losing precision across languages.

Opening an older database performs a transactional migration of structured values and links and invalidates compiled guests that used the old codec. The migration preserves event sequence numbers and verifies references before committing. Older databases with recorded effects whose descriptors were never stored, or pending handlers, require explicit recovery before migration: opening fails without changing the database rather than risking duplicate external effects. Back up the database before upgrading.

## Crate ownership

| Crate | Responsibility |
| --- | --- |
| `loom-proto` | Shared values, signatures, protocol, DAG-CBOR and TS declarations |
| `loom-store` | CAS, SQLite event history, projections, names, snapshots, effects |
| `loom-check` | Language checking and definition identity |
| `loom-build` | Component compiler sidecars and build cache |
| `loom-guest-rs`, `loom-guest-macros` | Synchronous Rust guest API and exports |
| `loom-rt` | Wasmtime fibers, actors, effects, machines |
| `loom-maintenance` | Backups, bounded index and build-cache maintenance |
| `loom-process`, `loom-model` | Supervised process execution and configurable model requests |
| `loom-api` | Shared service and HTTP/WebSocket transport |
| `loom-mcp` | MCP tools, prompts and resources |
| `loom-cli`, `loomd` | Terminal client and server entrypoint |

The WIT contract is in `loom-wit/handler.wit`. Guest code reaches the host through `loom:host/abilities.perform`; ambient WASI imports trap. Folds cannot perform effects.

## Verify

```sh
cargo test --workspace --locked
(cd loom-checker && bun test)
(cd loom-guest-ts && bun test)
(cd loom-ui && bun run check && bun run build)
LOOM_TOKEN="$LOOM_TOKEN" bun scripts/e2e.ts
LOOM_TOKEN="$LOOM_TOKEN" bun scripts/mcp-e2e.ts
./scripts/acceptance.sh
./scripts/dag-cbor-check.sh
./scripts/nix-check.sh
```

The HTTP and MCP scripts require a running daemon and real language toolchains. They build and execute guest components. `acceptance.sh` reports how many complete specification milestones pass; a missing or failing milestone remains a failure. A passing unit test suite alone does not imply all 11 milestones are delivered. Debug builds of the Wasmtime compiler are substantially slower than release builds when compiling a new TS component.

## Container

```sh
export LOOM_TOKEN='replace-with-your-token'
podman compose -f deploy/compose.yaml up --build
```

The image includes the compiler sidecars and static UI. Data and build cache live under `/data`. The compose file binds the service to host loopback. Use the backup command for a consistent SQLite snapshot while the server is running. A plain copy of an active SQLite file may omit WAL data.

`scripts/container-smoke.sh` builds the image and checks both guest languages over HTTP and MCP, a vendored Rust crate, build sandbox isolation, and clean SIGTERM shutdown. The Compose configuration unmasks the outer container's `/proc` paths so nested build namespaces can mount private procfs; builds retain their isolated network, filesystem, and cleared environment. `scripts/remote-check.sh acceptance` runs the milestone suite on the configured Linux development node within an 8-core, 24-GB systemd user unit.
