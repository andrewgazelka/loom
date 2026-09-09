# Loom

Loom runs TypeScript and Rust WebAssembly components behind one Rust actor host. Definitions, component bytes, event payloads, and effect results are content-addressed with BLAKE3. SQLite WAL holds the append-only event history and rebuildable indexes.

## Run locally

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

Actors export `run` and `fold` in TS or implement `loom::Actor` under `#[loom::actor]` in Rust. `examples/` contains executable fixtures. Actor commands include `spawn`, `send`, `state`, `fork`, and `upgrade`. Cross-language calls use the same CBOR values and host effect dispatcher.

Client responses contain `ok`, `seq`, `result`, and `diagnostics`. Results above 8 KB become CAS references. Use the `resolve` command to retrieve a reference. WebSocket clients connect to `/v1/stream` and send `{ "token": "...", "after": 0 }` as their first message; the server streams durable events after that cursor.

## Crate ownership

| Crate | Responsibility |
| --- | --- |
| `loom-proto` | Shared values, signatures, protocol, CBOR and TS declarations |
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
```

The HTTP and MCP scripts require a running daemon and real language toolchains. They build and execute guest components. `acceptance.sh` reports how many complete specification milestones pass; a missing or failing milestone remains a failure. A passing unit test suite alone does not imply all 11 milestones are delivered. Debug builds of the Wasmtime compiler are substantially slower than release builds when compiling a new TS component.

## Container

```sh
export LOOM_TOKEN='replace-with-your-token'
podman compose -f deploy/compose.yaml up --build
```

The image includes the compiler sidecars and static UI. Data and build cache live under `/data`. The compose file binds the service to host loopback. Use the backup command for a consistent SQLite snapshot while the server is running. A plain copy of an active SQLite file may omit WAL data.

`scripts/container-smoke.sh` builds the image and exercises both guest languages through its HTTP and MCP endpoints. `scripts/remote-check.sh acceptance` runs the milestone suite on the configured Linux development node within an 8-core, 24-GB systemd user unit.
