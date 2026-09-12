# Loom

Loom is a Rust execution runtime. Ordinary Rust functions perform algebraic effects
(`loom::sleep`, filesystem reads, model calls) on WebAssembly fibers: a call suspends
the fiber, a host handler does the work, the fiber resumes with the result, and
signatures stay ordinary Rust throughout. Actors (`loom-actor`) add durable,
supervised state: one Turso (SQLite-in-Rust) file per actor, one message per
transaction, OTP-parity supervision trees.

## Try it

```sh
nix run .                 # starts loomd, prints the token file path
nix run . -- --stdio      # same daemon, MCP over stdio for a coding agent
cargo test -p loom-actor  # 22 tests: the actor engine, standalone
```

Open `http://127.0.0.1:8787` and paste the printed token for the browser REPL.
See [docs/guide.md](docs/guide.md) for running without Nix and for the HTTP API.

## Effects

Calling an effect performs it. A handler can supply a value, forward to an outer
handler, or keep a one-shot continuation to resume later:

```rust
use loom::sleep;

#[loom::def(effects = ["sleep"])]
pub fn main() {
    loom::scope(|s| {
        let a = s.spawn(|| sleep(100)).expect("spawn");
        let b = s.spawn(|| sleep(200)).expect("spawn");
        a.join().expect("sleep");
        b.join().expect("sleep");
    });
}
```

The two scoped children run concurrently, so their sleeps overlap; `scope` waits for
both before returning. The guest-handler round trip (install, dispatch, resume,
remove) measured **13.811 µs median, 24.356 µs p99** over 10,000 warm calls on Linux,
September 10, 2026. Reproduce with `bun scripts/bench/effects-handlers.ts`. See
[docs/guide.md](docs/guide.md) for handler installation and
[content-addressed handlers](docs/content-addressed-handlers.md).

## Actors

An actor is one file: a mailbox (`inbox`), a behavior (content-hashed, in
`code_changes`), and domain tables the behavior owns. Handling one inbox message
runs inside one transaction that commits domain writes, effect records, and outbox
rows together; nothing is delivered until that transaction commits.

```mermaid
sequenceDiagram
    participant A as A.handle
    participant FA as A.db
    participant Pump
    participant FB as B.db
    A->>FA: sql() writes + cx.send(B, msg)
    FA->>FA: COMMIT (domain rows, outbox row, cursor)
    Pump->>FA: read undelivered outbox
    Pump->>FB: INSERT OR IGNORE inbox (keyed, exactly once)
    FB->>FB: COMMIT, mark outbox row delivered
    Note over FB: B.handle runs on the next pass
```

A handler error (or panic) is a `Trap`: the message rolls back, a `dead_letters` row
is written, and the actor parks. A supervisor restarts it by `resume` (retry the same
message, after a code change), `skip`, or `reset` (fresh file, same id):

```mermaid
graph TD
    S[Supervisor] --> A[Worker A running]
    S --> B0[Worker B running]
    B0 -->|trap at seq 5| B1[Worker B parked]
    B1 -->|promote new_hash| B2[Worker B parked, new code]
    B2 -->|restart resume| B3[Worker B running, retries seq 5]
```

A `Behavior` is a plain Rust value:

```rust
use async_trait::async_trait;
use loom_actor::{Behavior, Ctx, Trap};

pub struct Counter;

#[async_trait]
impl Behavior for Counter {
    fn hash(&self) -> &str {
        "counter-v1"
    }

    fn schema(&self) -> &str {
        "CREATE TABLE IF NOT EXISTS entries(seq INTEGER, body BLOB)"
    }

    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        if msg == b"poison" {
            return Err(Trap::new("counter rejects poison"));
        }
        cx.sql("INSERT INTO entries(seq, body) VALUES (?1, ?2)", turso::params![cx.seq(), msg]).await?;
        Ok(())
    }
}
```

Strategies (`one_for_one`, `one_for_all`, `rest_for_one`, `dynamic`), links, monitors,
forking a live actor at a past `seq` for validation, and `node.promote_where` for a
hot-load sweep across every actor on a hash are in
[docs/actors-turso.md](docs/actors-turso.md).

## Hook in over MCP

`loomd` also serves actors as MCP tools, so a coding agent can inspect and drive the
supervision tree directly:

| tool | does |
| --- | --- |
| `actor_list()` | every actor: id, status, behavior hash, cursor, inbox length, parent |
| `actor_tree(root?)` | nested tree from a root (default the node's root supervisor) |
| `actor_info(id)` | status, reason, cursor, deferred/inbox length, links, monitors, children |
| `actor_send(id, key?, msg)` | inject a keyed message, run to idle, return the new cursor |
| `actor_spawn(behavior_hash, init, parent?, spec?)` | spawn under a parent, return its id |
| `actor_stop(id, reason)` | stop with a reason |
| `actor_restart(id, verb)` | `resume` \| `skip` \| `reset` |
| `actor_promote(id, behavior_hash, author, rationale)` | append a `code_changes` row |
| `actor_promote_where(old_hash, new_hash, author, rationale)` | promote every actor on `old_hash` |
| `actor_lineage(id)` | the actor's `code_changes` rows |
| `actor_dead_letters(id)` | its trapped messages |
| `actor_fork(id, at_seq)` | copy the actor's state as of `at_seq` into a new, undeliverable fork |
| `actor_validate(id, candidate_hash, k, assertions?)` | replay the last `k` messages under `candidate_hash` on a fork, report `Matched`/`DivergedAt`/`Differs`/`Trapped` |
| `actor_sql(id, query, params?)` | read-only inspection; a write statement is refused |
| `actor_whereis` / `actor_register` / `actor_members` | name and group lookup |
| `actor_behaviors()` | registered behavior hashes, one line each |
| `actor_run()` | run every actor to idle, return messages processed |

Resources: `actor://<id>/inbox`, `.../effects`, `.../outbox`, `.../lineage`, and
`actor://tree`.

```sh
bun scripts/configure-codex-mcp.ts --token-file /path/to/loom/token
```

## Layout

| crate | is |
| --- | --- |
| `loom-actor` | one Turso file per actor, the pump, supervision |
| `loom-api` | HTTP/WebSocket service: auth, CAS browsing, the REPL API |
| `loom-build` | core wasm builder |
| `loom-check` | effect/language checking before a definition becomes executable |
| `loom-cli` | command-line client for a running `loomd` |
| `loom-guest-macros` | `#[loom::def]` / `#[loom::actor]` proc macros |
| `loom-guest-rs` | synchronous guest interface to the host, for Rust definitions |
| `loom-maintenance` | garbage collection over derived indexes only; CAS and event log untouched |
| `loom-mcp` | MCP server exposing the API as tools |
| `loom-model` | OpenAI-compatible model provider boundary |
| `loom-process` | durable process lifecycle for sandboxed machine commands |
| `loom-proto` | shared protocol types |
| `loom-rt` | the effect host runtime: filesystem, machine execution, shared core |
| `loom-store` | SQLite-backed event log and content-addressed store |
| `loomd` | the daemon binary: UI, API, and MCP endpoints |

Top level: `crates/` (above), `docs/`, `examples/`, `scripts/`, `deploy/`, `nix/`,
`ui/` (Svelte browser REPL), `rustc/` (Rust
guest toolchain build).

## Docs

[docs/guide.md](docs/guide.md) covers the HTTP API, non-Nix setup, and isolation
model. [docs/actors-turso.md](docs/actors-turso.md) is the actor spec in full,
including OTP parity. [docs/content-addressed-handlers.md](docs/content-addressed-handlers.md)
covers stored handler definitions. [scripts/bench/README.md](scripts/bench/README.md#reproduce)
reproduces the largest-file-scan benchmark described there.
