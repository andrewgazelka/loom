# Loom

Loom executes Rust core WebAssembly definitions in `loom-rt`. Definitions, core wasm bytes, and effect results are content-addressed with BLAKE3 in `loom-store`. Actors run in `loom-actor`, with one Turso file per actor; `loom-behavior` runs Loom definitions inside their transactions.

**Memory isolation is a security requirement. Rust safety checks are not a formally proven boundary against adversarial code.** Compiler and library soundness bugs can expose undefined behavior through safe Rust; denying `unsafe` does not close that class of bug. Loom keeps separate Wasm memories and exchanges DAG-CBOR values instead of sharing guest pointers. Wasm validation, the engine, and checked host interfaces remain trusted, and the complete system has no end-to-end formal proof. See the [memory isolation decision](plan-unified-memory.md#memory-isolation-decision) for the concrete Rust soundness issue and supporting sources.

The long-term goal is a formally verified guest language, compiler, and runtime contract. If their checked guarantees cover direct memory interaction, we can revisit the isolation design on that basis. Until then, separate Wasm memories and DAG-CBOR remain the execution model.

## Try it

Start the browser REPL, HTTP API, and MCP endpoint on `127.0.0.1:8793`:

```sh
nix run --builders '' .#repl -- --bind 127.0.0.1:8793
```

The launcher prints `Token file: <path>` and opens `http://127.0.0.1:8793/#token=<token>`. The `CLI: <store path>/bin/loom` line identifies the matching CLI. Put its directory on `PATH`, export `LOOM_TOKEN` from the token file, and set `LOOM_URL=http://127.0.0.1:8793`. Pass `--url "$LOOM_URL"` to the CLI as shown in the README. Follow the [README command sequence](../README.md#try-it) to add, view, run, update, and replay the guests in `examples/unison/`.

The proof checks these results in order:

1. `loom add examples/unison/greet.rs --lang rust --name greet` returns a definition hash and an empty inferred effect row.
2. `loom view <hash>` returns the original submitted source byte-for-byte and two items after the input file has been moved away. The proof reads the fixture before moving it and compares `view.source` against those original bytes.
3. `loom run greet '"loom"'` returns `"hello, loom"` with an empty effects list.
4. Updating with `greet-v2.rs` only renames a local and keeps the hash. Updating with `greet-v3.rs` changes the greeting constant and moves the hash. The old hash still runs; `history greet` contains both hashes.
5. Adding `sleeper.rs` infers exactly `["sleep"]` through a trait method on a generic. The source has no effect declaration.
6. Adding `counter.rs`, spawning it, and sending three messages leaves its cursor at 3.
7. Adding `counter-v2.rs` preserves the effect row. Validating three messages under that candidate returns `Differs` and unequal original/fork hashes for the `counter` table.
8. Promotion with rationale `e2e` puts both behavior hashes in lineage. One more message moves the cursor to 4.
9. The HTTP MCP companion repeats checks 1–8 under separate names and actors, then checks discovery of the 14 definition tools, 22 actor tools and the `command` wrapper. It prints its own `N/9`.

Stop the daemon on the test port and commit your changes before running the automated version. This machine’s Nix requires a committed Git input:

```sh
LOOM_E2E_PORT=8793 scripts/e2e-unison.sh
```

Install `nix`, `bun`, and `curl` on `PATH` first. The shell takes the matching CLI directory from the launcher’s `CLI:` line. `LOOM_E2E_PORT` defaults to 8787 and drives the occupancy check, launcher bind, and `LOOM_URL`. The example uses 8793 to leave the live REPL on 8787 alone. The shell refuses an occupied port, creates a fresh `LOOM_DATA_DIR` and build directory, starts `nix run --builders '' .#repl -- --bind 127.0.0.1:$LOOM_E2E_PORT`, reads the printed token file, and uses disposable fixture copies. It stops its daemon on exit and retains state and logs at the printed path. The final stdout line is `N/9`; success requires `9/9` and exit status zero. Prerequisite failures mark dependent checks as blocked. The initial build timeout defaults to 3600 seconds (`LOOM_E2E_START_TIMEOUT`); each client operation defaults to 600000 milliseconds (`LOOM_E2E_OPERATION_TIMEOUT_MS`).

The TypeScript companion shares semantic assertions between the CLI and MCP paths and reuses `scripts/mcp-client.ts` for authenticated Streamable HTTP. This avoids adding a Rust test binary just to drive JSON transports. To run the MCP companion against an already running daemon, copy the fixtures into a disposable directory and pass that directory:

```sh
fixtures=$(mktemp -d)
cp examples/unison/*.rs "$fixtures/"
bun scripts/e2e-unison-mcp.ts --mcp "$fixtures"
```

MCP is also available over stdio with `nix run --builders '' .#repl -- --bind 127.0.0.1:8793 -- --stdio`; the automated proof uses HTTP. Every tool uses a bare verb and returns `{ok, seq, result, diagnostics}`. MCP carries this envelope in `structuredContent`. Add/update expose `result.hash` and `result.entries.<entry>.effects = {labels, unknown}`; view returns `result.source` and an item-name-to-hash map; run returns `result.output` and `result.effects`; history returns an array of revisions with `hash`. Every inferred-row check requires `unknown: false`. Product failures are reported as `PRODUCT <step> <what>` after separating them from script mistakes.

## Run locally

On Apple Silicon macOS or x86-64 Linux, Nix supplies the daemon, Svelte app, the pinned Rust compiler that guest definitions are built with, and the content-hashing rustc driver, prebuilt:

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

Install Bun 1.3.13 and the pinned guest compiler, `nightly-2026-08-24`: definitions are built by that compiler and identified by the `tools/hash-rustc` driver, which is a rustc plugin and therefore only links against the compiler that runs it. The daemon compares `hash-rustc -vV` with `$RUSTC -vV` and refuses a mismatch. Linux machine execution also needs Bubblewrap. The builder rebuilds the standard library for `wasm32-unknown-unknown` with atomics enabled.

```sh
rustup toolchain install nightly-2026-08-24 \
  --component rustc-dev --component llvm-tools --component rust-src \
  --target wasm32-unknown-unknown
cargo +nightly-2026-08-24 build --release --manifest-path tools/hash-rustc/Cargo.toml
export RUSTC="$(rustup which --toolchain nightly-2026-08-24 rustc)"
export LOOM_HASH_RUSTC="$PWD/tools/hash-rustc/target/release/hash-rustc"
(cd ui && bun install --frozen-lockfile && bun run build)
export LOOM_TOKEN='replace-with-your-token'
cargo run --release -p loomd -- --db loom.sqlite
```

`loomd` itself builds with any recent Rust; only the two variables above decide what guests are compiled by. Under Nix, `nix/loom.sh` exports the same two, pointing at store paths.

Open <http://127.0.0.1:8787> and enter the same token. The daemon serves the built Svelte application, HTTP API, WebSocket event stream, and MCP endpoint. The token authorizes the single owner; keep the listener on loopback unless network access is intended.

```sh
cargo run -p loom-cli -- --token "$LOOM_TOKEN" run sum '[20,22]'
cargo run -p loomd -- --db loom.sqlite --stdio
```

The second command starts the MCP stdio transport. Configure an MCP client to run it with `LOOM_TOKEN` and an absolute `--root` pointing at this checkout. Streamable HTTP MCP is at `/mcp` and requires the same bearer token.

## Command reference

The CLI, HTTP commands, and MCP tools use the same definition operations. `add` defaults to TypeScript; use `--lang javascript` or `--lang rust` explicitly. File extensions do not select a language, and `update` retains the existing definition language. The CLI reads files for `add` and `update`; the service stores their source in CAS. `view` reads that stored source, including when the original file has changed or disappeared.

| CLI | MCP tool | Result |
| --- | --- | --- |
| `add <file> [--lang language] [--name n] [--dep alias=name-or-hash]...` | `add` | Name, definition hash, entries, and backend metadata; each `--dep` pins `use alias::...` to a definition |
| `view <name-or-hash>` | `view` | Stored source and item table |
| `update <name> <file> [--expected_hash hash]` | `update` | Atomic caller propagation or a durable repair session; old hashes remain runnable |
| `history <name>` | `history` | Hash chain, timestamps, and changed items between entries |
| `diff <old-hash> <new-hash>` | `diff` | Added, removed, and changed items, with their hashes |
| `run <name-or-hash> [args-json]` | `run` | Output and recorded effects |
| `find <text>` | `find` | Matching names and item names |
| `dependents <hash>` | `dependents` | Definitions with a dependency pinned to the hash |
| `export <name>... --out <file>` | `export` | One CARv1 bundle holding the named definitions, their dependency closure, sources, identities and vendored crate trees ([format](bundles.md)) |
| `import <file> [--into prefix]` | `import` | Rebuilds every bundled definition from source and binds the bundle's names, optionally under `prefix/`; all or nothing |

```sh
loom --token "$LOOM_TOKEN" add sum.rs --lang rust --name sum
loom --token "$LOOM_TOKEN" run sum '[20,22]'
loom --token "$LOOM_TOKEN" view sum
loom --token "$LOOM_TOKEN" update sum sum.rs
loom --token "$LOOM_TOKEN" history sum
loom --token "$LOOM_TOKEN" add caller.rs --lang rust --name caller --dep sum=sum
loom --token "$LOOM_TOKEN" export caller --out caller.car
loom --token "$LOOM_TOKEN" --url "$OTHER_LOOM_URL" import caller.car --into friend
```

`--dep alias=value` is repeatable (`--deps` is the same flag). A 64-character hexadecimal value is used as the pinned hash; any other value is a definition name that the server resolves to its current hash at admission, on every transport. An unknown name is an error naming it. The caller writes `use alias::path::Item;`; the stored definition records the resolved hashes.

`export` collects the transitive `deps` closure of every target and stores the bundle as a raw CAS object; the CLI downloads it to `--out`, which must not exist. A target given as a hash must be the current value of a name. `import` uploads the file to `POST /v1/cas`, then runs the `import` verb with the returned reference: every block is hashed against its CID, every definition is rebuilt through the same admission path as `add` in dependency order, and the whole bundle is refused when a rebuilt hash differs from the recorded one (the error names both hashes and both toolchain hashes) or when a bound name already points at a different hash. `--into friend` binds `friend/<name>` instead. Nothing reaches the live store unless every definition rebuilt; see [bundles.md](bundles.md) for the byte-level format and the object list.

Updates and repair sessions are described in [the scripting guide](../examples/evolution/README.md). Inspect `result.update.status`; an accepted command may still need repairs. `update_repair <id> <revision> <changes-json>` submits a batch, `update_view <id>` reads the latest revision, and `update_rebase <id> <revision>` retries after disjoint namespace changes.

Guest Rust is ordinary Rust: built-in derives (`Clone`, `Copy`, `Debug`, `Default`, `Eq`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`), the standard macros (`vec!`, `format!`, `matches!`, `assert!`, `write!`, `panic!`, `todo!`, `unreachable!`, and their siblings), `loom::serde_json::json!`, and your own `macro_rules!` all work; macros expand inside rustc before identity and effects are computed. Procedural macros from crates (`#[derive(Serialize)]`, `#[tokio::main]`), `println!`/`dbg!` (the guest has no stdio), `line!`/`file!`, and `cfg` other than `cfg(test)` are refused with `LOOM_MACRO`, and the diagnostic names the item and lists what is available; the full table and its reasons are in [content-addressed code](content-addressed-code.md#macros-in-guest-source). There are no export attributes. Every crate-root `pub fn` is an entry. An optional schema is declared as `pub const LOOM_SCHEMA: &str`; effect rows are inferred by the compiler driver. There are no effect declarations. `add` reports each entry’s inferred row in `entries.<name>.effects`, with `labels` and `unknown` fields.

```rust
pub fn sum(a: i64, b: i64) -> i64 { a + b }
```

A source file may expose several entries. `run <name>` selects the matching public function in a named definition, or a unique entry with that name among currently named definitions. Ambiguous names report the matching definition hashes. `run <definition hash>` requires a sole entry and otherwise reports the candidate names. Each entry hash is also addressable: `run <entry hash>` executes that entry, and `view <entry hash>` returns its owning definition and identifies the selected entry. An unchanged entry shared by several revisions retains its earliest published owner. Private helpers and nested functions are not entries.

HTTP clients post `{ "command": "run", "args": { "target": "sum", "args": [20,22] } }` to `/v1/command` with the bearer token. `add` takes `source`, an optional `name`, and an optional `deps` object mapping aliases to definition names or hashes; `update` takes `name` and `source`. The remaining definition arguments are `target` for `view`, `name` for `history`, `old` and `new` for `diff`, `text` for `find`, `hash` for `dependents`, `targets` (an array of names or hashes) for `export`, and `bundle` (the raw CAS reference returned by `POST /v1/cas`) plus an optional `into` prefix for `import`. `export` returns `result.bundle.$ref`; fetch the bytes from `GET /v1/cas/{cid}`.

Item hashes describe compiler-resolved definitions. Renaming a local variable or reformatting source leaves them unchanged. A changed helper can change its callers' hashes too. The definition hash is a BLAKE3 Merkle root over the sorted export path/hash pairs from the driver, where an export is any definition reachable through `pub` visibility from the crate root: root entries, nested public functions, public traits and types with their impls. Every export contributes, so changing a nested public function that no entry calls still changes the definition hash, while unchanged items retain their own resolved-HIR hashes. A reference into another definition carries that item's stored hash, so a dependent's hash follows the content it uses, not the dependency's crate build. Alpha-renaming a local leaves both the definition hash and history unchanged; changing a constant changes the hash. Source revisions have their own BLAKE3 hashes. A published definition pins its executable and schema; a conflicting publication is rejected. The Wasm and toolchain hashes identify its executable build. Builds require the `hash-rustc` driver and reject missing identity outputs. Stores without `defs.behavior_hash` are rejected by column name.

### Compiler resolution

Loom uses one compiler resolver for every guest operation. There are two modes:

- **`LOOM_HASH_RUSTC` is set:** provide absolute executable paths in both `LOOM_HASH_RUSTC` (the prebuilt driver) and `RUSTC` (the guest compiler). Loom requires the driver’s reported rustc version to equal the complete output of `$RUSTC -vV`. It uses the supplied driver as-is: driver resolution never looks for `tools/hash-rustc`, reads a source pin, runs rustup, or invokes Cargo. Missing overrides, relative paths, unavailable executables, and version mismatches fail with named errors. The guest sysroot comes from `$RUSTC --print sysroot`; guest Cargo comes from `<guest-sysroot>/bin/cargo`.
- **`LOOM_HASH_RUSTC` is unset:** no environment setup is needed. Loom reads `<root>/tools/hash-rustc/rust-toolchain.toml`, resolves that channel’s rustc and Cargo through `rustup which --toolchain <channel>`, and builds the driver from `tools/hash-rustc/Cargo.toml` with that pinned compiler. `RUSTC` defaults to the pinned rustc; an explicit override must report the same version. Cargo checks the cached driver’s freshness on subsequent builds.

The Rust API’s `with_driver_path` selects the same prebuilt mode and takes precedence over `LOOM_HASH_RUSTC`. There is no automatic driver search on `PATH` and no fallback to plain rustc. Prebuilt mode clears ambient `RUSTUP_TOOLCHAIN`; guest commands put the selected sysroot’s binaries first on `PATH`.

`<root>` is the service source root (`LOOM_ROOT` in the packaged launcher). `<build-dir>` is `LOOM_BUILD_DIR`, defaulting to `<root>/.loom-build`. Source-built drivers are cached at `<build-dir>/hash-rustc/<blake3(rustc -vV)>/release/hash-rustc`.

A Nix package may omit `tools/` and rustup when supplying both overrides. Ship the prebuilt driver’s runtime libraries, the matching guest compiler with Cargo and `rust-src`, and the runtime SDK sources/build scripts. Cargo is still used for guest dependency preparation and compilation; the prebuilt mode never invokes Cargo to build or resolve the driver.

```sh
export LOOM_HASH_RUSTC="/nix/store/<driver>/bin/hash-rustc"
export RUSTC="/nix/store/<matching-nightly-sysroot>/bin/rustc"
loomd
```

### Actor commands

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

Rust guests use deterministic DAG-CBOR at the core wasm effect boundary. Structured CAS values use the same codec; core wasm binaries, source bundles, and other raw bytes retain the raw codec. JSON clients represent a link as exactly `{ "$ref": "<CID>" }`. In DAG-CBOR this becomes tag 42 containing the zero-prefixed binary CID. Local links use CIDv1 with a BLAKE3-256 digest and distinguish DAG-CBOR (`0x71`) from raw bytes (`0x55`). Definition identities are Merkle roots over sorted export path/hash pairs; individual items retain their resolved-HIR hashes, and source revisions use separate source hashes.

Maps have string keys ordered by encoded length and then bytes. Decoders reject duplicate keys, nonminimal or indefinite encodings, other tags, malformed CIDs, undefined, nonfinite floats, and trailing bytes. Floats use 64 bits. The shared JSON value model encodes safe integral numbers as integers; Rust integers outside JavaScript's safe range are rejected instead of losing precision across languages. The isolated-call payload between two Rust guests is outside that model: the host never decodes it, so it keeps full 64-bit integers and `loom::Bytes` byte strings, and the refusal happens only where the bytes meet a JSON consumer.

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
| `loom-guest-rs` | Synchronous Rust guest API; entries are plain crate-root `pub fn` items, so there is no guest macro crate |
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

Calling an effect performs it: `loom::sleep(100)` suspends until its timer finishes, and `loom::perform::<T>(label, args)` performs a custom effect. Concurrent Rust work on the shared-core path uses `loom::scope`, `scope.spawn(|| ...)`, and `child.join()`. Use `loom::spawn(|| ...)` with `'static` captures for fire-and-forget work or a `JoinHandle` moved into another task. Dropping that handle leaves its task running; the host cancels unfinished detached tasks when the definition entry returns, without an implicit wait. Joining a trapped detached task returns its error; an unjoined detached failure is discarded. A synchronous `loom::isolated::call(DEF, args)` can run inside a scoped child.

## Isolated calls

Code calls code by static linking: a definition dependency compiles as an rlib and its functions are ordinary typed Rust calls. `loom::isolated::call` is something else: it runs another definition in a fresh wasm instance with its own memory, and exists only for the isolation boundary (actor boundaries, untrusted code, another language). `const DEF: loom::isolated::Def<fn(u32) -> u32> = loom::isolated::Def::new("<hash>")` names the callee by hash (`"$self"` names the running definition; `.entry("name")` selects an export; `Def::from_hex` takes a run-time hash), and `loom::isolated::call(DEF, 7)` returns `Result<u32, loom::CallError>`. `Def<fn(A, B) -> R>` through eight arguments takes a tuple; the signature is the caller's belief, checked only when the callee decodes the payload (`CallError::Decode`) or the host compares the declared arity with the stored export before instantiating anything (`CallError::Arity`). `CallError` is a structured enum: `NotFound`, `Denied`, `Trapped`, `Decode`, `Arity`, `DepthExceeded`. Nesting stops at 64 levels, before the callee is instantiated.

The wire is a fixed header (definition hash as 32 raw bytes, entry name, argument count) followed by the arguments as one DAG-CBOR array encoded directly from the typed values, with no `Value` tree in between. The host reads the header and never decodes the payload; it hashes the bytes for trace identity. The result comes back the same way. That is two codec passes for the arguments (caller encodes, callee decodes) and two for the result, down from eleven when every hop rebuilt a `serde_json::Value`. Because the host does not interpret the payload, `u64` and `i64` keep their full range and `loom::Bytes` travels as a CBOR byte string; only results that leave the Rust boundary (the host `run` API, a JavaScript caller) must fit the JSON value model, and the boundary refuses them there. Guest handler frames never see an isolated call: the callee runs in its own memory and the call itself is not a `perform`; its inferred effect label is `call`.

Definition signatures record the residual effect row: the labels that can reach the outermost host handler. The compiler infers this row through resolved calls, including the concrete implementations selected by trait and generic calls. The host-enforced `allowed_effects` policy is a separate permission limit. Omitting `allowed_effects` adds no policy restriction; `[]` permits none. A publication pins its policy with the executable; a different policy for the same definition hash is rejected. Cross-definition calls inherit the intersection of caller and callee permissions, including on cache hits. Scoped children inherit the caller's permissions.

The Effects view shows individual invocations and their outcomes. To capture file content changes from a process, pass `capture_paths: ["note.txt"]` to `exec` or `process.start`. Paths are resolved within the process root; the capture records actual before/after bytes in CAS and displays created, modified, and deleted files as diffs. Capture is limited to 64 explicitly selected regular files, at most 1 MiB each. Symlinks, unsupported files, and unavailable reads are reported explicitly.

A completed call records one content-addressed trace and a `call_completed` event. Each trace occurrence identifies its job scope, effect and outcome; result blobs deduplicate across calls. The Effects view expands trace pages into individual recorded invocations.

Use `call.replay` with the original definition `hash`, `args`, and recorded call `scope` to replay a completed successful or failed call. Replay checks the definition and argument identity, consumes the recorded occurrences, and verifies the final outcome, including recorded errors. Cancelled calls cannot be resumed through `call.replay`. A fresh call executes its external effects again unless an effect has a valid global memoization key.

Machine filesystem effects resolve from a pinned root directory handle. Parent traversal and symlinks cannot redirect resolution outside that root. `fs.list` supports guest-driven traversal; `fs.walk` performs a bounded traversal in the host. Both return the same typed entry values. Persisted root identity prevents a restart from silently accepting a replacement directory.

These snapshots observe selected files across the process interval. They do not enumerate every write, track metadata-only changes, or distinguish concurrent writers. Historical effects without snapshots remain browsable, with no invented diff.

Dependencies remain pinned when a definition name moves. `update` retains its existing dependency pins and effect policy unless replacements are supplied. Use `--dep alias=<name-or-hash>` (repeatable) to replace pins and `--allowed_effects '[]'` to deny effects; explicit `null` clears the effect policy. Use `dependents <hash>` to find callers and update each caller explicitly. Actor behavior changes use `promote` or MCP `promote`.

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
