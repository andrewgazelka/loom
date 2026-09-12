# Loom

Loom executes Rust core WebAssembly definitions in `loom-rt`. Definitions, core wasm bytes, and effect results are content-addressed with BLAKE3 in `loom-store`. Actors run in `loom-actor`, with one Turso file per actor; `loom-behavior` runs Loom definitions inside their transactions.

**Memory isolation is a security requirement. Rust safety checks are not a formally proven boundary against adversarial code.** Compiler and library soundness bugs can expose undefined behavior through safe Rust; denying `unsafe` does not close that class of bug. Loom keeps separate Wasm memories and exchanges DAG-CBOR values instead of sharing guest pointers. Wasm validation, the engine, and checked host interfaces remain trusted, and the complete system has no end-to-end formal proof. See the [memory isolation decision](plan-unified-memory.md#memory-isolation-decision) for the concrete Rust soundness issue and supporting sources.

The long-term goal is a formally verified guest language, compiler, and runtime contract. If their checked guarantees cover direct memory interaction, we can revisit the isolation design on that basis. Until then, separate Wasm memories and DAG-CBOR remain the execution model.

## Run locally

On Apple Silicon macOS or x86-64 Linux, Nix supplies the daemon, Svelte app, and Rust guest toolchain:

```sh
nix run .#repl
```

This starts loomd, prints the token file path, and prints and opens
`Loom REPL: http://127.0.0.1:8787/#token=<token>` in your browser (skipped,
with a log line, when no display is available). The token rides the URL
fragment, never a query string, so it never reaches the server or its
logs; with `--tokens-file` there is no single token to embed, so the URL
has no fragment and you enter one from the file yourself. The database,
token, and build caches persist under `~/Library/Application Support/loom`
on macOS or `~/.local/share/loom` on Linux. `XDG_DATA_HOME` changes the
base directory; `LOOM_DATA_DIR` sets the complete directory. The first Nix
build downloads and compiles dependencies.

Pass daemon options after `--`, for example `nix run . -- --bind 127.0.0.1:8788` or `nix run . -- --stdio` for an MCP client. Set `LOOM_TOKEN` to choose a token instead of generating one. Linux process isolation uses the packaged Bubblewrap; machine execution remains platform-dependent.

The package installs two programs. `loomd` is the daemon with its state directory, token, and guest toolchain arranged for it, and it is what `nix run .` and `nix run .#repl` start. `loom` is the CLI that talks to a running daemon; `nix shell . -c loom --help` puts it on `PATH` for one command, and `nix build .` leaves both in `result/bin`. The REPL launcher prints the CLI's store path on its second line.

### Development without Nix

Install Rust 1.97, its `rust-src` component, and Bun 1.3.13. Linux machine execution also needs Bubblewrap. The builder rebuilds the standard library for `wasm32-unknown-unknown` with atomics enabled.

```sh
rustup component add rust-src
rustup target add wasm32-unknown-unknown
(cd ui && bun install --frozen-lockfile && bun run build)
export LOOM_TOKEN='replace-with-your-token'
cargo run --release -p loomd -- --db loom.sqlite
```

Open <http://127.0.0.1:8787> and enter the same token. The daemon serves the built Svelte application, HTTP API, WebSocket event stream, and MCP endpoint. The token authorizes the single owner; keep the listener on loopback unless network access is intended.

```sh
cargo run -p loom-cli -- --token "$LOOM_TOKEN" run sum '[20,22]'
cargo run -p loomd -- --db loom.sqlite --stdio
```

The second command starts the MCP stdio transport. Configure an MCP client to run it with `LOOM_TOKEN` and an absolute `--root` pointing at this checkout. Streamable HTTP MCP is at `/mcp` and requires the same bearer token.

## Command reference

The CLI, HTTP commands, and MCP tools use the same definition operations. The CLI reads files for `add` and `update`; the service stores their source in CAS. `view` reads that stored source, including when the original file has changed or disappeared.

| CLI | MCP tool | Result |
| --- | --- | --- |
| `add <file.rs> [--name n]` | `add` | Name, definition hash, entry item hash, Wasm hash, and item table |
| `view <name-or-hash>` | `view` | Stored source and item table |
| `update <name> <file.rs>` | `update` | New definition and name binding; old hash remains runnable |
| `history <name>` | `history` | Hash chain, timestamps, and changed items between entries |
| `diff <old-hash> <new-hash>` | `diff` | Added, removed, and changed items, with their hashes |
| `run <name-or-hash> [args-json]` | `run` | Output and recorded effects |
| `find <text>` | `find` | Matching names and item names |
| `dependents <hash>` | `dependents` | Definitions with a dependency pinned to the hash |

```sh
loom --token "$LOOM_TOKEN" add sum.rs --name sum
loom --token "$LOOM_TOKEN" run sum '[20,22]'
loom --token "$LOOM_TOKEN" view sum
loom --token "$LOOM_TOKEN" update sum sum.rs
loom --token "$LOOM_TOKEN" history sum
```

Guest Rust has no macros. Every crate-root `pub fn` is an entry. An optional schema is declared as `pub const LOOM_SCHEMA: &str`; effect rows are inferred by the compiler driver. There are no effect declarations. `add` reports each entry’s inferred row in `entries.<name>.effects`, with `labels` and `unknown` fields.

```rust
pub fn sum(a: i64, b: i64) -> i64 { a + b }
```

A source file may expose several entries. `run <name>` selects the matching public function in a named definition, or a unique entry with that name among currently named definitions. Ambiguous names report the matching definition hashes. `run <hash>` requires a sole entry and otherwise reports the candidate names. Private helpers and nested functions are not entries.

HTTP clients post `{ "command": "run", "args": { "target": "sum", "args": [20,22] } }` to `/v1/command` with the bearer token. `add` takes `source` and an optional `name`; `update` takes `name` and `source`. The remaining definition arguments are `target` for `view`, `name` for `history`, `old` and `new` for `diff`, `text` for `find`, and `hash` for `dependents`.

Item hashes describe compiler-resolved definitions. Renaming a local variable or reformatting source leaves them unchanged. A changed helper can change its callers' hashes too. The definition hash is the driver’s resolved-HIR entry hash. Alpha-renaming a local leaves both the definition hash and history unchanged; changing a constant changes the hash. Source revisions have their own BLAKE3 hashes. A published definition pins its executable and schema; a conflicting publication is rejected. The Wasm and toolchain hashes identify its executable build. Builds require the `hash-rustc` driver and reject missing identity outputs. Stores without `defs.behavior_hash` are rejected by column name.

Actor commands use the same names, argument schemas, admission checks, and response envelope on CLI, HTTP, and MCP:

| CLI | MCP tool |
| --- | --- |
| `spawn <def> [init]` | `spawn` |
| `send <id> <msg>` | `send` |
| `tree` | `tree` |
| `info <id>` | `info` |
| `lineage <id>` | `lineage` |
| `validate <id> <candidate> <k>` | `validate` |
| `promote <id> <hash> --author <name> --rationale <text>` | `promote` |
| `fork <id> <seq>` | `fork` |
| `actors` | `actors` |
| `stop <id> <reason>` | `stop` |
| `restart <id> <verb>` | `restart` |
| `dead_letters <id>` | `dead_letters` |
| `sql <id> <query> [--params <json>]` | `sql` |
| `whereis <name>` | `whereis` |
| `register <name> <id>` | `register` |
| `members <group>` | `members` |
| `behaviors` | `behaviors` |
| `promote_where <old> <new> --author <name> --rationale <text>` | `promote_where` |
| `drain` | `drain` |

Actor behaviors resolve from stored definitions when `spawn` runs; a running node can spawn a newly added definition by name or hash. Actor entries accept one `Vec<u8>` message, with optional `LOOM_SCHEMA` SQL. Each actor pins its resolved definition hash. Each actor owns its domain tables, inbox, effects, and outbox in one Turso file. See [Actors on Turso](actors-turso.md) for transactions, supervision, and behavior changes.

All responses, including MCP actor responses and failures, use `{ok, seq, result, diagnostics}`. `spawn` defaults omitted `init` to `null` everywhere. Promotions require `author` and `rationale`; validation uses an unsigned 32-bit `k`. `send` returns a cursor on success; a trapped message returns `ok: false` with its actor id, message sequence, and cause. WebSocket clients connect to `/v1/stream` and send `{ "token": "...", "after": 0 }` as their first message; the server streams durable events after that cursor.

## DAG-CBOR and links

Rust guests use deterministic DAG-CBOR at the core wasm effect boundary. Structured CAS values use the same codec; core wasm binaries, source bundles, and other raw bytes retain the raw codec. JSON clients represent a link as exactly `{ "$ref": "<CID>" }`. In DAG-CBOR this becomes tag 42 containing the zero-prefixed binary CID. Local links use CIDv1 with a BLAKE3-256 digest and distinguish DAG-CBOR (`0x71`) from raw bytes (`0x55`). Definition identities are resolved-HIR entry hashes; source revisions use separate source hashes.

Maps have string keys ordered by encoded length and then bytes. Decoders reject duplicate keys, nonminimal or indefinite encodings, other tags, malformed CIDs, undefined, nonfinite floats, and trailing bytes. Floats use 64 bits. The shared JSON value model encodes safe integral numbers as integers; Rust integers outside JavaScript's safe range are rejected instead of losing precision across languages.

Filesystem results have typed Rust SDK values. `DirEntry` has named `name`, `size`, and `kind` fields and encodes as a DAG-CBOR array. Host-produced results decode directly in the guest without rebuilding and re-encoding an intermediate value. Foreign bytes whose content determines a hash still receive strict canonical validation. HTTP and MCP envelopes use JSON; the guest boundary and structured CAS payloads use DAG-CBOR.

Compiled core modules carry an effect-protocol version. Modules built against the earlier map-shaped filesystem results must be rebuilt before execution; their historical source and CAS records remain readable. This prevents an old guest decoder from silently interpreting the new result shape.

Stores containing retired actor tables are rejected at open with an error naming the table. There is no migration of those stores.

## Crate ownership

| Crate | Responsibility |
| --- | --- |
| `loom-proto` | Shared values, signatures, protocol and DAG-CBOR |
| `loom-store` | CAS, definitions, names, call traces, effect results |
| `loom-check` | Language checking and definition identity |
| `loom-build` | Rust compiler sidecars and build cache |
| `loom-guest-rs`, `loom-guest-macros` | Synchronous Rust guest API and exports |
| `loom-rt` | Wasmtime fibers, definition calls, effects, machine filesystem roots |
| `loom-actor`, `loom-behavior` | Turso actors and transactional Loom definitions |
| `loom-maintenance` | Backups, bounded index and build-cache maintenance |
| `loom-process`, `loom-model` | Supervised process execution and configurable model requests |
| `loom-api` | Shared service and HTTP/WebSocket transport |
| `loom-mcp` | MCP tools, prompts and resources |
| `loom-cli`, `loomd` | Terminal client and server entrypoint |

Rust definitions built through `add` use the [shared-core ABI](shared-core-abi.md), with `loom.perform` dispatching through guest handlers to the outermost host handler.

## Verify

```sh
cargo test --workspace --locked
(cd ui && bun run check && bun run build)
LOOM_TOKEN="$LOOM_TOKEN" bun scripts/e2e.ts
LOOM_TOKEN="$LOOM_TOKEN" bun scripts/mcp-e2e.ts
./scripts/acceptance.sh
./scripts/dag-cbor-check.sh
./scripts/nix-check.sh
```

The HTTP and MCP scripts require a running daemon and the Rust guest toolchain. They build and execute guest modules. `acceptance.sh` reports how many complete specification milestones pass; a missing or failing milestone remains a failure. A passing unit test suite alone does not imply all 11 milestones are delivered.

The two API guest integration tests are ignored by default because they compile Rust guests and need an executable that handles compiler-cache requests. Run them with the matching Rust compiler and `rust-src` installed:

```sh
cargo build -p loom-build --example build_smoke
LOOM_BUILD_DIR="$(mktemp -d)" \
LOOM_COMPILER_CACHE_OWNER="$PWD/target/debug/examples/build_smoke" \
cargo test -p loom-api --lib -- --include-ignored --test-threads=1
```

## Container

```sh
export LOOM_TOKEN='replace-with-your-token'
podman compose -f deploy/compose.yaml up --build
```

The image includes the Rust compiler sidecars and static UI. Data and build cache live under `/data`. The compose file binds the service to host loopback. Use the backup command for a consistent SQLite snapshot while the server is running. A plain copy of an active SQLite file may omit WAL data.

`scripts/container-smoke.sh` builds the image and checks Rust guest definitions over HTTP and MCP, a vendored Rust crate, build sandbox isolation, and clean SIGTERM shutdown. The Compose configuration unmasks the outer container's `/proc` paths so nested build namespaces can mount private procfs; builds retain their isolated network, filesystem, and cleared environment. `scripts/remote-check.sh acceptance` runs the milestone suite on the configured Linux development node within an 8-core, 24-GB systemd user unit.

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

The runner creates a separate 10,000-file fixture, starts Codex with only Loom configured and a read-only shell sandbox, and retains its real JSONL trace. The six-gate verifier checks model-written Rust definitions, independent results, filesystem changes, and five-scan warm medians below 1,500 ms. It ignores unrelated user configuration for this isolated run. Do not point it at the live application database.

## Effects and file changes

Calling an effect performs it: `loom::sleep(100)` suspends until its timer finishes, and `loom::perform::<T>(label, args)` performs a custom effect. Concurrent Rust work on the shared-core path uses `loom::scope`, `scope.spawn(|| ...)`, and `child.join()`. Use `loom::spawn(|| ...)` with `'static` captures for fire-and-forget work or a `JoinHandle` moved into another task. Dropping that handle leaves its task running; the host cancels unfinished detached tasks when the definition entry returns, without an implicit wait. Joining a trapped detached task returns its error; an unjoined detached failure is discarded. A synchronous `loom::call(DEF, args)` can run inside a scoped child.

Definition signatures record the residual effect row: the labels that can reach the outermost host handler. The compiler infers this row through resolved calls, including the concrete implementations selected by trait and generic calls. The host-enforced `allowed_effects` policy is a separate permission limit. Omitting `allowed_effects` adds no policy restriction; `[]` permits none. A publication pins its policy with the executable; a different policy for the same entry hash is rejected. Cross-definition calls inherit the intersection of caller and callee permissions, including on cache hits. Scoped children inherit the caller's permissions.

The Effects view shows individual invocations and their outcomes. To capture file content changes from a process, pass `capture_paths: ["note.txt"]` to `exec` or `process.start`. Paths are resolved within the process root; the capture records actual before/after bytes in CAS and displays created, modified, and deleted files as diffs. Capture is limited to 64 explicitly selected regular files, at most 1 MiB each. Symlinks, unsupported files, and unavailable reads are reported explicitly.

A completed call records one content-addressed trace and a `call_completed` event. Each trace occurrence identifies its job scope, effect and outcome; result blobs deduplicate across calls. The Effects view expands trace pages into individual recorded invocations.

Use `call.replay` with the original definition `hash`, `args`, and recorded call `scope` to replay a completed successful or failed call. Replay checks the definition and argument identity, consumes the recorded occurrences, and verifies the final outcome, including recorded errors. Cancelled calls cannot be resumed through `call.replay`. A fresh call executes its external effects again unless an effect has a valid global memoization key.

Machine filesystem effects resolve from a pinned root directory handle. Parent traversal and symlinks cannot redirect resolution outside that root. `fs.list` supports guest-driven traversal; `fs.walk` performs a bounded traversal in the host. Both return the same typed entry values. Persisted root identity prevents a restart from silently accepting a replacement directory.

These snapshots observe selected files across the process interval. They do not enumerate every write, track metadata-only changes, or distinguish concurrent writers. Historical effects without snapshots remain browsable, with no invented diff.

Dependencies remain pinned when a definition name moves. `update` retains its existing dependency pins and effect policy unless replacements are supplied. Use `--deps '{"alias":"<hash>"}'` to replace pins and `--allowed_effects '[]'` to deny effects; explicit `null` clears the effect policy. Use `dependents <hash>` to find callers and update each caller explicitly. Actor behavior changes use `promote` or MCP `promote`.

## Guest-defined effect handlers

`loom::handle_any(handler, body)` installs a deep handler around an ordinary Rust
closure. The handler receives an `Effect` and a one-shot `Continuation`, then returns
`Reply::Resume(value)`, `Reply::Forward`, or `Reply::Deferred`.

```rust
use loom::{Continuation, Effect, Reply, Value};

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

For content-addressed reuse, see [stored handler definitions](content-addressed-handlers.md).

### Residual effect rows

Effect rows are inferred. Calling `loom::sleep(100)` adds `sleep`; calling `loom::perform("custom.label", args)` adds `custom.label`. Trait dispatch follows the implementation selected for that entry's concrete types. An unused implementation that calls `exec` does not add `exec` to the entry's row.

A total `loom::handle(["sleep"], handler, body)` removes `sleep` from the body's row. Effects performed by the handler itself remain in the outer row. `handle_any` may forward, so it does not remove labels. A pinned `handle_with` uses the stored handler's residual row and any stored total-handling labels.

Effect rows are inferred through the resolved call graph. `perform` accepts a string literal or a const evaluated by rustc, such as `const L: &str = "custom.label";`. Dynamic labels are rejected at the call site with `effect label at <span> is not a literal or const; rows are inferred and need a static label`. Sandboxing is omission: total handlers remove handled labels from the residual host row, and the host refuses every effect absent from that inferred row.

Caller permissions still apply. If the residual row omits `exec`, the guest cannot reach the host's exec implementation.

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
