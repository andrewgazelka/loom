# Loom

Loom runs TypeScript and Rust WebAssembly components behind one Rust actor host. Definitions, component bytes, event payloads, and effect results are content-addressed with BLAKE3. SQLite WAL holds the append-only event history and rebuildable indexes.

**Memory isolation is a security requirement. Rust safety checks are not a formally proven boundary against adversarial code.** Compiler and library soundness bugs can expose undefined behavior through safe Rust; denying `unsafe` does not close that class of bug. Loom keeps separate Wasm memories and exchanges DAG-CBOR values instead of sharing guest pointers. Wasm validation, the engine, and checked host interfaces remain trusted, and the complete system has no end-to-end formal proof. See the [memory isolation decision](plan-unified-memory.md#memory-isolation-decision) for the concrete Rust soundness issue and supporting sources.

The long-term goal is a formally verified guest language, compiler, and runtime contract. If their checked guarantees cover direct memory interaction, we can revisit the isolation design on that basis. Until then, separate Wasm memories and DAG-CBOR remain the execution model.

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

Actors export `run` and `fold` in TS or implement `loom::Actor` under `#[loom::actor]` in Rust. `examples/` contains executable fixtures. Guest actor effects use `loom::actor::send(actor, msg)` and `loom::actor::spawn(DEF, state)` in Rust, or `actor.send(actorId, msg)` and `actor.spawn(def, state)` from the TS `actor` object. Their effect labels are `actor.send` and `actor.spawn`. Actor commands include `spawn`, `send`, `state`, `fork`, and `actor.upgrade`. Cross-language calls use the same DAG-CBOR values and host effect dispatcher.

Client responses contain `ok`, `seq`, `result`, and `diagnostics`. Results above 8 KB become CAS references. Use the `resolve` command to retrieve a reference. WebSocket clients connect to `/v1/stream` and send `{ "token": "...", "after": 0 }` as their first message; the server streams durable events after that cursor.

## DAG-CBOR and links

The Rust and TS guests use deterministic DAG-CBOR at the WIT boundary. Structured CAS values use the same codec; component binaries, source bundles, and other raw bytes retain the raw codec. JSON clients represent a link as exactly `{ "$ref": "<CID>" }`. In DAG-CBOR this becomes tag 42 containing the zero-prefixed binary CID. Local links use CIDv1 with a BLAKE3-256 digest and distinguish DAG-CBOR (`0x71`) from raw bytes (`0x55`). Definition identities remain source hashes.

Maps have string keys ordered by encoded length and then bytes. Decoders reject duplicate keys, nonminimal or indefinite encodings, other tags, malformed CIDs, undefined, nonfinite floats, and trailing bytes. Floats use 64 bits. The shared JSON value model encodes safe integral numbers as integers; Rust integers outside JavaScript's safe range are rejected instead of losing precision across languages.

Filesystem results have typed Rust SDK values. `DirEntry` has named `name`, `size`, and `kind` fields and encodes as a DAG-CBOR array. Host-produced results decode directly in the guest without rebuilding and re-encoding an intermediate value. Foreign bytes whose content determines a hash still receive strict canonical validation. HTTP and MCP envelopes use JSON; the guest boundary and structured CAS payloads use DAG-CBOR.

Compiled components carry an effect-protocol version. Components built against the earlier map-shaped filesystem results must be rebuilt before execution; their historical source and CAS records remain readable. This prevents an old guest decoder from silently interpreting the new result shape.

Opening an older database performs a transactional migration of structured values and links and invalidates compiled guests that used the old codec. The migration preserves event sequence numbers and verifies references before committing. Older databases with recorded effects whose requests were never stored, or pending handlers, require explicit recovery before migration: opening fails without changing the database rather than risking duplicate external effects. Back up the database before upgrading.

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

Rust definitions built through `loom_define` use the [shared-core ABI](shared-core-abi.md), with `loom.perform` dispatching through guest handlers to the outermost host handler. Component definitions use `loom-wit/handler.wit` and `loom:host/effects.perform`; ambient WASI imports trap. Folds can handle effects locally, but an effect reaching the host is refused.

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

### Codex over MCP

Configure the four Loom tools in Codex, preserving other server settings:

```sh
bun scripts/configure-codex-mcp.ts --token-file /path/to/loom/token
```

The launcher prints the token path: macOS defaults to `~/Library/Application Support/loom/token`; Linux defaults to `${XDG_DATA_HOME:-~/.local/share}/loom/token`. The setup helper reports only the configured endpoint and approved tool count, never the bearer header.

The token file must have owner-only permissions. The configuration stores its bearer header in an owner-only file and approves the four Loom tools; these tools can define and execute guest code. Use `--url` and `--config` to select another endpoint or configuration file.

To verify a fresh Codex session against an **isolated test daemon**, set its URL and token file explicitly:

```sh
LOOM_URL=http://127.0.0.1:18891 LOOM_TOKEN_FILE=/path/to/test-state/token bun scripts/codex-mcp-run.ts
```

The runner creates a separate 10,000-file fixture, starts Codex with only Loom configured and a read-only shell sandbox, and retains its real JSONL trace. The six-gate verifier checks model-written TS and Rust definitions, independent results, filesystem changes, and five-scan warm medians below 1,500 ms. It ignores unrelated user configuration for this isolated run. Do not point it at the live application database.

## Effects and file changes

Calling an effect performs it: `loom::sleep(100)` suspends until its timer finishes, and `loom::perform::<T>(label, args)` performs a custom effect. Concurrent Rust work on the shared-core path uses `loom::scope`, `scope.spawn(|| ...)`, and `child.join()`. Use `loom::spawn(|| ...)` with `'static` captures for fire-and-forget work or a `JoinHandle` moved into another task. Dropping that handle leaves its task running; the host cancels unfinished detached tasks when the definition entry returns, without an implicit wait. Joining a trapped detached task returns its error; an unjoined detached failure is discarded. A synchronous `loom::call(DEF, args)` can run inside a scoped child. TypeScript uses `perform<T>(op, args)` and plain effect functions. Rust examples built as WIT components and TypeScript definitions have no concurrency API; both execute effect calls sequentially.

Definition signatures distinguish inferred effects, the host-enforced `allowed_effects` policy, and effects observed during execution. Inference is conservative: dynamic calls, getters, iterators, and unexpanded Rust code can leave the set unknown. Omitting `allowed_effects` permits all host effects; `[]` permits none. Explicit policies are part of definition identity. Cross-definition calls inherit the intersection of caller and callee permissions, including on cache hits. Scoped children inherit the caller's permissions.

The Effects view shows individual invocations and their outcomes. To capture file content changes from a process, pass `capture_paths: ["note.txt"]` to `exec` or `process.start`. Paths are resolved within the process root; the capture records actual before/after bytes in CAS and displays created, modified, and deleted files as diffs. Capture is limited to 64 explicitly selected regular files, at most 1 MiB each. Symlinks, unsupported files, and unavailable reads are reported explicitly.

A completed call records one content-addressed trace and a `call_completed` event. Each trace occurrence identifies its job scope, effect and outcome; result blobs deduplicate across calls. Actors retain recovery checkpoints. The Effects view expands trace pages and keeps historical per-effect events readable. Opening an older store migrates recoverable effect records transactionally and refuses ambiguous records without partially applying the migration.

Use `call.replay` with the original definition `hash`, `args`, and recorded call `scope` to replay a completed successful or failed call. Replay checks the definition and argument identity, consumes the recorded occurrences, and verifies the final outcome, including recorded errors. Cancelled calls cannot be resumed through `call.replay`; actor recovery uses its saved checkpoint. A fresh call executes its external effects again unless an effect has a valid global memoization key.

Machine filesystem effects resolve from a pinned root directory handle. Parent traversal and symlinks cannot redirect resolution outside that root. `fs.list` supports guest-driven traversal; `fs.walk` performs a bounded traversal in the host. Both return the same typed entry values. Persisted root identity prevents a restart from silently accepting a replacement directory.

These snapshots observe selected files across the process interval. They do not enumerate every write, track metadata-only changes, or distinguish concurrent writers. Historical effects without snapshots remain browsable, with no invented diff.

Crate intake uses `loom --token "$LOOM_TOKEN" crate add serde@1.0.210` or the MCP `crate_add` tool with `{name, version}`. The registry checksum is verified before the source tree enters the CAS. Pin the returned hash in a Rust bundle's manifest:

```toml
[loom.crates]
serde = { hash = "<returned 64-digit hash>", features = ["derive"] }
```

Updating a definition name leaves existing dependency hashes intact. Run `loom --token "$LOOM_TOKEN" upgrade <old-hash> <new-hash>` or MCP `loom_upgrade` to rewrite named dependents explicitly. Both definition hashes and crate source hashes use this command; its result lists the changed identities. Actor behavior changes remain a separate `actor.upgrade` command with `{actor, hash}`.

## Guest-defined effect handlers

`loom::handle_any(handler, body)` installs a deep handler around an ordinary Rust
closure. The handler receives an `Effect` and a one-shot `Continuation`, then returns
`Reply::Resume(value)`, `Reply::Forward`, or `Reply::Deferred`.

```rust
use loom::{Continuation, Effect, Reply, Value};

#[loom::def(effects = [])]
pub fn main() {
    loom::handle(["sleep"], |_effect: Effect, _k: Continuation| {
        Reply::Resume(Value::Null)
    }, || loom::sleep(200).expect("sleep failed"))
    .expect("handler failed");
}
```

`handle` promises to handle its selected effects. Returning `Forward`
for one of those effects aborts the execution. Use `handle_any` for a handler that
examines arbitrary effects and may forward. Forwarding continues at the next
outer frame. Effects made inside a handler callback also start below that
frame, so a logging handler can perform its own I/O without calling itself.
The suspended body's continuation retains the installed handler: this is the
meaning of a deep handler.

A scoped child inherits the parent's handler stack. A call into another
stored definition has a separate execution memory and does not inherit guest
handlers. Handler closures may borrow values, but must be `Send`. The runtime
serializes calls to each mutable handler, and drains inherited children before
releasing borrowed handler storage. A handler trap aborts its execution and
identifies the frame.

`Reply::Deferred` lets a handler retain its continuation and resume it later.
`Continuation::resume(value)` consumes the continuation. `abandon()` explicitly
aborts the suspended performer; dropping an unresolved deferred continuation
fails with `continuation dropped`. Continuations cannot be cloned or resumed
more than once. Retaining one forever prevents progress and is bounded by the
execution deadline. A handler must return before it can be invoked again; do
not wait inside a mutable callback for another invocation of the same frame.

Recording observes only the outermost host handler. A guest-handled effect
is guest computation and creates no root-effect record. If that handler reads
a real file, the read reaches the host and is recorded. Replay reruns the guest
handlers and supplies their recorded root-effect results. Root recording and
replay are implemented by the same host handler chain used for execution.
Pure actor folds may install handlers, but any effect reaching the root
still fails.

For content-addressed reuse, see [stored handler definitions](content-addressed-handlers.md).

### Residual effect declarations

`#[loom::def(effects = ["sleep"])]` declares the effect names that the host must
supply. `loom-check` rejects known residual effects outside that set. A total
`handle` removes its selected effects from the body's inferred row;
effects performed by the handler itself remain in the outer row. Unknown
dispatch requires an explicit declaration, which the runtime enforces at the
root. Actor definitions use `#[loom::actor(effects = [...])]`.

This is Loom's conservative source analysis and runtime capability check, not
an effect type system inside rustc. A declaration does not grant additional
host capabilities: it intersects the caller's allowed effects. Omitting `exec`
from the permitted root row prevents guest code from reaching the host's exec
implementation even through dynamic dispatch.

### Preview filesystem writes

`loom::preview::writes(body)` is a guest handler for `fs.write`, `fs.read`, and
`fs.read_optional`. Writes update an in-memory overlay; reads in the body see
that overlay. Repeated writes produce one before/final-after change, and
unchanged content produces no change. The returned `Preview` includes the
body's result and content-addressed before/after values, rendered by the REPL's
filesystem changes view. See [the complete preview example](../examples/rust-preview/src/lib.rs).

Only those filesystem effects are intercepted. Reads used to capture the
original file and CAS writes remain real root effects. Other effects and
calls into separate definitions are not automatically previewed. Restrict the
root effect row when the body must not execute other external effects.
