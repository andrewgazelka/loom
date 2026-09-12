# Actors on Turso: one file per actor

Status: design v2, 2026-09-11. Supersedes the pasted "Actor = one SQLite file" note.
Scope of v1: one machine, native Rust behaviors, no wasm, no capabilities, no cells.
Rust only. No component model, no WIT, no TypeScript.

## The goal, as a command

    cargo test -p loom-actor

prints `test result: ok. 9 passed` when v1 is done. Baseline 2026-09-11: the crate
does not exist (0/9). The nine tests are listed at the end; each is a property, not a
scenario name.

## What an actor is

An actor is: one Turso database file (`<dir>/<actor_id>.db`), one mailbox (the
`inbox` table in that file), and one behavior (a Rust value implementing `Behavior`,
addressed by a content hash, recorded in the file's `code_changes` table).

State lives in the file. The behavior is stateless over the file. There is no
"replay from the last checkpoint": the file is always the full state. A crash
mid-message rolls back one transaction and re-runs one message.

## The one invariant: one message = one transaction

Handling inbox row `seq` runs inside one Turso transaction that contains, and only
commits together:

1. the handler's own `sql()` writes to domain tables,
2. `effects` rows for every short effect the handler performed,
3. `outbox` rows for every send / spawn / long-effect request,
4. `meta.cursor := seq`.

Nothing is delivered and nothing external happens inside the transaction, except
short effects (see below), which are idempotent by key. After COMMIT the pump
delivers outbox rows. A receiver can only ever observe committed state.

## Tables in every actor file

    meta(key TEXT PRIMARY KEY, value TEXT)             -- id, parent, cursor, status
    inbox(seq INTEGER PRIMARY KEY, key TEXT UNIQUE, sender TEXT, msg BLOB, received_at INTEGER)
    effects(seq INTEGER, idx INTEGER, kind TEXT, request BLOB, result BLOB, PRIMARY KEY(seq, idx))
    outbox(seq INTEGER, idx INTEGER, target TEXT, msg BLOB, delivered INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(seq, idx))
    code_changes(seq INTEGER PRIMARY KEY, behavior_hash TEXT NOT NULL, parent_hash TEXT, author TEXT, rationale TEXT, schema_sql TEXT)
    dead_letters(seq INTEGER PRIMARY KEY, msg BLOB, error TEXT, at INTEGER)
    children(id TEXT PRIMARY KEY, spawned_seq INTEGER, behavior_hash TEXT)
    snapshots(seq INTEGER PRIMARY KEY, path TEXT)     -- history v1, see below

Domain tables are whatever `schema_sql` in `code_changes` rows created. `inbox`,
`effects`, `outbox`, `code_changes`, `dead_letters` are append-only.

`inbox.key` is the dedupe key: `"<sender_id>:<sender_seq>:<idx>"` for actor sends,
`"req:<seq>:<idx>"` for long-effect results, caller-supplied for external injection.
INSERT OR IGNORE on the key makes at-least-once delivery exactly-once in effect.

`status` in meta is one of `running`, `parked`, `stopped`. Every value names its
leaver: `parked` leaves on a `code_changes` row or a `skip` command; `stopped` never
leaves (the file stays for reading).

## Behavior

    #[async_trait]
    pub trait Behavior: Send + Sync {
        fn hash(&self) -> &str;                       // content hash, the identity used in code_changes
        fn schema(&self) -> &str;                     // SQL run once when this behavior is first promoted
        async fn handle(&self, cx: &mut Ctx, msg: &[u8]) -> Result<(), Trap>;
    }

`Ctx` is the only thing a handler can touch:

    cx.sql(sql, params) -> Result<Rows>     // runs on the actor's open transaction
    cx.send(target_id, msg)                  // outbox row, target = actor id
    cx.spawn(behavior_hash, init_msg) -> child_id   // outbox row + children row; id is derived (below)
    cx.request(kind, req) -> request_id      // long effect: outbox row, target = "effect:<kind>"
    cx.effect(kind, req) -> result           // short effect: runs now, keyed by (actor, seq, idx), recorded in effects
    cx.now() -> i64                          // wall clock, is a short effect (recorded)
    cx.random(n) -> Vec<u8>                  // derived from (actor_id, seq, counter), not recorded
    cx.seq() -> i64

A `Trap` is a handler error the runtime treats as deterministic (a returned `Err`
or a panic caught at the boundary). Environmental failures (Turso I/O error) are
runtime errors: the transaction rolls back and the message is retried, never
poisoned.

Behaviors are looked up by hash in a `Registry` (`HashMap<String, Arc<dyn Behavior>>`)
owned by the `Node`. Wiring a Loom wasm definition in is a later lane: it will be one
`Behavior` impl whose `handle` calls the definition and maps Loom effects onto `Ctx`.

## Effects: short vs long, and idempotency

Short effects run inside the handler through a host `EffectHandler` trait
(`async fn call(&self, key: &EffectKey, kind: &str, req: &[u8]) -> Result<Vec<u8>>`).
The key `(actor_id, seq, idx)` is passed to the handler; any effect with external
side effects must use it as its idempotency key, because the `effects` row is inside
the transaction and a crash after the external call and before COMMIT re-runs the
message and re-performs the effect with the same key. The `effects` table exists for
audit and for validation replay, not for dedupe.

Long effects are two-phase: `cx.request` writes an outbox row with target
`effect:<kind>`; the pump hands it to the `EffectHandler` after commit; the result is
injected into the actor's inbox with key `req:<seq>:<idx>`. The handler instance may
die in between; nothing is held in memory.

## The pump

After each commit the node reads undelivered outbox rows for that actor and, per row:

- target = actor id: `INSERT OR IGNORE INTO inbox` of the target file (opening it if
  needed), then mark delivered.
- target = `spawn`: create the child file at its derived id, insert its first
  `code_changes` row, run `schema()`, insert the init message; mark delivered.
  Creation is idempotent: if the file exists, skip to marking delivered.
- target = `effect:<kind>`: call the EffectHandler; inject the result as an inbox
  message; mark delivered.

Marking delivered is its own tiny transaction on the sender's file. Redelivery on a
crash is safe because every delivery is keyed.

## Ids

Root actors: ULID. Children: `blake3(parent_id || seq || idx)` hex, first 26 chars,
so respawning the same outbox row yields the same id. A namespace/cell prefix is
reserved as a fixed `"a0"` prefix on every id; it is not interpreted in v1.

## Supervision

A `Trap` on message `seq`:

1. ROLLBACK the message transaction.
2. In a new transaction: insert `dead_letters(seq, msg, error)`, then apply the
   actor's strategy (a meta key, default `park`):
   - `park`: set status `parked`; cursor stays at seq-1; the actor accepts inbox
     inserts but runs nothing until a `code_changes` row is promoted (then it retries
     seq) or a `skip` command advances the cursor.
   - `skip`: cursor := seq; continue.
   - `stop`: status `stopped`.
3. Send the parent (meta.parent, if any) an inbox message
   `{"type":"poison","child":id,"seq":seq,"error":...}`.

The runtime classifies: handler `Err`/panic = Trap. Turso errors = retry with
backoff, bounded by `max_retries` in node config, after which the message is treated
as a Trap with the error text (so nothing loops forever without a row saying so).

## Code changes

    node.promote(actor_id, behavior_hash, author, rationale)

inserts a `code_changes` row and runs the new behavior's `schema()` in the same
transaction. The next message runs the new behavior; a parked actor resumes. Rollback
is `promote` with an earlier hash. Lineage is `SELECT * FROM code_changes ORDER BY seq`.

## History and fork (v1: snapshots, not WAL frames)

Every `snapshot_every` messages (node config, default 64) the node, after commit,
runs `VACUUM INTO '<dir>/<id>.snap.<seq>.db'` (or Turso's equivalent backup call;
see the existence check in the runbook) and inserts a `snapshots` row.

    node.fork(actor_id, at_seq) -> fork_id

copies the latest snapshot with `snapshots.seq <= at_seq` to a new file
`<dir>/<fork_id>.db`, sets its meta id/parent, then replays inbox messages
`(snap_seq, at_seq]` with the behavior recorded in `code_changes` at each seq and
with short effects served from the `effects` table (an `EffectHandler` that answers
by `(seq, idx)` and errors on a miss). The fork's outbox rows are never delivered:
the fork has meta `status = fork` and the pump refuses that status.

Frame-level history (shipping Turso WAL frames to segments so any seq is reachable
without replay) is the v2 replacement for this section. It changes only how
`fork` builds the file, not the API.

## Validation of a candidate behavior

    node.validate(actor_id, candidate_hash, k) -> Verdict

1. `fork(actor_id, cursor - k)`, then promote `candidate_hash` on the fork.
2. Run inbox messages `(cursor-k, cursor]` on the fork with effects served from the
   original's `effects` table.
3. Verdict:
   - `Matched { tables: Vec<(name, hash)> }` when every effect matched and the fork's
     domain tables hash-equal the original's (per table: blake3 over
     `SELECT * ORDER BY rowid`).
   - `DivergedAt { seq, idx, expected: Vec<u8>, got: Vec<u8> }` when the candidate
     performed a different effect (kind or request) at (seq, idx), or one the log
     lacks. This is information, not failure.
   - `Trapped { seq, error }`.
   - `Differs { tables: Vec<(name, original_hash, fork_hash)> }` when effects matched
     but tables differ.
4. The caller (later: the supervising actor or an LLM lane) decides with an optional
   list of SQL assertions run on the fork, each `SELECT` expected to return 1.

## Node

    pub struct Node { dir: PathBuf, registry: Registry, effects: Arc<dyn EffectHandler>, config: Config }
    node.spawn_root(behavior_hash, init_msg) -> actor_id
    node.send(actor_id, key, msg)             // external injection, keyed
    node.run_until_idle()                     // drain every actor with cursor < max(inbox.seq) and status running; run the pump; repeat until nothing moves
    node.open(actor_id) -> Actor              // handle over one file
    node.promote / fork / validate / skip / stop

One open Turso connection per actor file, held in a `HashMap<ActorId, Arc<Mutex<Connection>>>`;
one message runs at a time per actor. Actors in the same node run concurrently across
files (tokio tasks), never within one file.

Pragmas per file at open: `journal_mode=WAL`, `synchronous=NORMAL`.

## What is deliberately not in v1

Capabilities (ids are the only handle; every actor in a node can address every
other), wasm behaviors, WAL frame shipping, blob storage, cells, leases, alarms
(`set_alarm` is a long effect of kind `alarm` served by the EffectHandler; the
default handler implements it with a tokio sleep, so it is in v1 as an effect, not as
a runtime feature).

## Tests (the goal number)

1. `three_messages_three_rows`: 3 messages each INSERT via `sql()`; file has 3 rows,
   cursor = 3.
2. `crash_before_commit_reruns_once`: inject a failure after the sql write and before
   commit on message 2 (a test EffectHandler that returns Err once for key (_,2,0));
   after `run_until_idle` the file has exactly 3 rows and `effects` has one row for
   seq 2, never two.
3. `trap_rolls_back_send`: A's handler sends to B then returns Err; B's inbox stays
   empty; A has one dead_letters row, status `parked`, cursor 0.
4. `send_delivers_exactly_once`: A sends to B in a message that commits; B receives one
   copy; a second pump pass (simulated restart with the outbox row marked
   undelivered) still yields one row in B's inbox.
5. `short_effect_keyed_and_recorded`: a counting EffectHandler sees key (A, 1, 0) once
   for a committed message; the effects table row equals the returned bytes.
6. `long_effect_result_arrives_as_message`: `cx.request("echo", b"x")` commits; after
   the pump, the actor's inbox has a message with key `req:1:0` and body `x`, and the
   handler runs on it.
7. `fork_matched`: after 6 messages with snapshot_every = 4, `validate(id, same_hash, 2)`
   returns `Matched`.
8. `fork_diverged`: a candidate that performs an extra short effect returns
   `DivergedAt { seq, idx, .. }` and the original file is byte-identical to before
   (compare table hashes), and the fork delivered nothing (B's inbox unchanged).
9. `promote_then_rollback_lineage`: promote H2 with a schema adding a column; the next
   message sees the column; promote H1 again; the next message runs H1; lineage query
   returns [H1, H2, H1]; a parked actor resumes on promote.

Each test is a `#[tokio::test]` in `crates/loom-actor/tests/`, on a `tempfile`
directory, with two fixture behaviors (`Counter`, `Forwarder`) in `tests/common/`.

## Addendum A (2026-09-11, same day): a real supervision tree

The sections above give a parent a `poison` message and a `children` table. That is
not a supervision tree. This addendum makes it one; it is part of v1.

### Vocabulary (OTP names, actor-file meaning)

- **link(a, b)**: bidirectional. When either stops for any reason other than `normal`,
  the other receives `{"type":"exit","from":id,"reason":..}`; if it does not trap
  exits (meta `trap_exit`, default false) the runtime stops it with the same reason,
  which propagates along its own links. Spawned children are linked to their parent by
  default (`spawn_link` semantics; `cx.spawn` takes `link: bool`, default true).
- **monitor(a, b)**: one-way. When `b` stops or is poisoned, `a` receives
  `{"type":"down","ref":ref,"from":b,"reason":..}` exactly once. Monitors are rows in
  the monitoring actor's file (`monitors(ref TEXT PRIMARY KEY, target TEXT)`) and in
  the target's (`monitored_by(ref, watcher)`), both written by the pump when the
  `monitor` outbox row is delivered. `demonitor(ref)` deletes both.
- **stop(id, reason)**: an outbox row a parent (or anyone holding the id) can emit;
  the pump sets the target's status to `stopped` with the reason, then fans out `exit`
  to links and `down` to monitors, then recursively stops the target's linked children.
  Reasons: `normal`, `shutdown`, `poison`, `killed`, or a free string.
- **child spec**: `spawn` takes `{behavior_hash, init, restart: permanent|transient|temporary, shutdown: brutal|timeout_ms, link: bool}`. Stored in the parent's `children` table.
- **restart** for a durable actor is one of three verbs, chosen by the supervisor:
  - `resume`: status back to `running`, cursor unchanged (retries the poison message).
  - `skip`: move the poison message to `dead_letters`, cursor past it, `running`.
  - `reset`: archive the file (`<id>.db` -> `<id>.reset.<n>.db`), create a fresh file
    with the same id, same `code_changes` head, same monitors/links rows, and the
    child's init message. This is OTP's fresh-state restart.
- **strategy**: `one_for_one` (restart the failed child), `one_for_all` (stop every
  sibling, then restart all in spec order), `rest_for_one` (stop and restart the
  failed child and every child spawned after it).
- **intensity**: `max_restarts` in `max_seconds` (default 3 in 5). Exceeding it stops the
  supervisor itself with reason `shutdown`, which propagates up its own link. Restart
  timestamps are rows in the supervisor's file (`restarts(child, at)`), so the window
  survives the supervisor's own restarts only if it is `resume`d, which is the point.

### The default supervisor is a normal behavior

`crates/loom-actor/src/supervisor.rs` ships `Supervisor` as a `Behavior` with hash
`"supervisor-v1"`. Its state is its own file: `spec(child_id, order, behavior_hash,
init, restart, shutdown, link)`, `restarts(child, at)`, and meta keys `strategy`,
`max_restarts`, `max_seconds`. It handles:

- `{"type":"start_child", spec}` -> spawns, records.
- `{"type":"down"|"exit"|"poison", ..}` from a child -> applies strategy + restart verb
  per the spec (`permanent`: always restart; `transient`: restart unless reason is
  `normal`; `temporary`: never), emitting `stop` and `spawn` outbox rows. The restart
  verb for `poison` is `resume` only if a `code_changes` row newer than the poison seq
  exists on the child, else `reset` for `permanent`, else `skip`.
- `{"type":"which_children"}` -> replies (via `cx.send` to the sender) with the spec
  rows and each child's status read through `node.open`.

A supervisor whose child is itself a supervisor is a tree; nothing special.

### Runtime pieces this adds

- Tables in every file: `monitors`, `monitored_by`, `links(peer TEXT PRIMARY KEY)`,
  `restarts` (supervisor only, but harmless everywhere).
- Outbox targets: `monitor:<id>`, `demonitor:<ref>`, `link:<id>`, `unlink:<id>`,
  `stop:<id>` (msg = reason), `spawn` (msg = child spec).
- Ctx: `cx.monitor(id) -> ref`, `cx.demonitor(ref)`, `cx.link(id)`, `cx.unlink(id)`,
  `cx.stop(id, reason)`, `cx.exit(reason)` (stop self after this message commits),
  `cx.sender() -> Option<ActorId>`, `cx.trap_exit(bool)`.
- Node: `node.stop(id, reason)`, `node.restart(id, verb)`, `node.tree(root) ->
  Vec<(depth, id, status, behavior_hash)>` (walks `children`), `node.register(name,
  id)` / `node.whereis(name)` (a `names` table in `<dir>/_node.db`, the only node-level
  file; it holds nothing an actor file holds).
- Stopping is transactional per file and idempotent: `stop` on a `stopped` actor is a
  no-op that still marks the outbox row delivered.

### Tests 10 to 13 (goal becomes N/13)

10. `monitor_delivers_down_once`: A monitors B; B `cx.exit("normal")`; A's inbox has
    exactly one `down` with the ref and reason, also after a second pump pass.
11. `link_cascades_stop`: A spawns B (linked), B spawns C (linked); B traps with
    strategy `stop`; A (not trapping) and C both end `stopped` with reason `poison`;
    with `trap_exit` true on A, A stays `running` and holds an `exit` message.
12. `one_for_one_reset_then_intensity`: a Supervisor with `max_restarts=2 in 60 s` and
    one permanent child whose behavior traps on every message: after three poison
    rounds the child has been `reset` twice (files `<id>.reset.1.db`, `.2.db` exist)
    and the supervisor is `stopped` with reason `shutdown`; its parent (a monitoring
    test actor) holds one `down`.
13. `rest_for_one_order`: supervisor with children [A, B, C] in that order,
    strategy `rest_for_one`; B fails; C is stopped then respawned (new file generation),
    A is untouched (same cursor, no reset file); `which_children` reply lists A, B, C
    with B and C `running`.

## Addendum B (2026-09-11): parity with Erlang/OTP, decided primitive by primitive

Bar: every guarantee an Erlang process, GenServer or Supervisor gives has a named
equivalent here, or a written reason it cannot and what replaces it. Nothing below is
optional in v1 except where marked "later" with its leaver.

| OTP | Decision here | Guarantee kept |
|---|---|---|
| `spawn`, `spawn_link`, `spawn_monitor` | `cx.spawn(spec)`; `link` default true; `monitor: bool` in the spec installs a monitor in the same outbox row so no window exists where the child runs unobserved | yes |
| `send` (async, unbounded, per-pair FIFO) | outbox rows delivered in `(seq, idx)` order; delivery to a target that is unreachable blocks only later rows for that target, never other targets; inbox is a seq log (disk-bounded, not memory-bounded) | yes, pair FIFO |
| signal ordering: a `DOWN`/`EXIT` from A arrives after every message A sent | when an actor stops, the pump drains its remaining outbox rows first, then fans out `exit` and `down` | yes |
| `receive` at any point in a function | a handler is one `receive`: the top of `handle`. Blocking mid-function is not supported; the continuation is the next message with state in the file. This is a CPS transform, equal in power, different in ergonomics. When Loom wasm behaviors land, a fiber may suspend across messages only through a long effect | by transform |
| selective receive (non-matching messages stay in the mailbox) | `cx.defer()`: the current message stays in `inbox` with state `deferred`, cursor does not advance past it; after the next message that commits without `defer`, deferred rows are re-presented in seq order before newer rows (stash/unstash). A message deferred N times in a row with no intervening commit is a `Trap` (livelock is an error, never a spin) | yes |
| `receive ... after T` | `cx.send_after(self, T, msg) -> timer_ref`, `cx.cancel_timer(ref)`, `cx.read_timer(ref)`; timers are rows in the sender's file plus one `alarm` long effect; cancel before fire deletes the row and the fire is ignored by the pump | yes |
| `exit(pid, reason)`, `exit(self(), reason)` | `cx.stop(id, reason)`, `cx.exit(reason)` (self, after this message commits) | yes |
| `exit(pid, kill)` untrappable, becomes `killed` | reason `kill` bypasses `trap_exit` and the `terminate` hook; observers see `killed` | yes |
| `Process.flag(:trap_exit, true)` | `cx.trap_exit(bool)` meta key; with it, `exit` from a link is an inbox message; without it, the runtime stops the actor with the same reason (reason `normal` from a link is ignored, as in Erlang) | yes |
| `link/unlink`, `monitor/demonitor` (`flush`) | outbox targets `link:`, `unlink:`, `monitor:`, `demonitor:`; `demonitor(ref, flush: true)` also deletes any undelivered `down` for that ref from the inbox | yes |
| `register/unregister/whereis`, `:global` | `_node.db` `names(name PRIMARY KEY, id)`; a registered actor that stops is unregistered by the pump | yes (single node) |
| `:pg` / `Registry` duplicate keys | `_node.db` `groups(group, id)`; `node.join/leave/members`; leaving on stop is the pump's job | yes |
| `Process.info`, `is_process_alive` | `node.info(id) -> {status, reason, cursor, inbox_len, deferred_len, behavior_hash, links, monitors, parent, children}` | yes |
| process dictionary | domain tables | yes |
| `hibernate` | eviction: closing the connection is free; the file is the state | yes |
| `max_heap_size`, reductions | meta keys `memory_max`, `fuel` recorded in v1, enforced when the wasm behavior lane lands (its leaver: that lane's test "fuel exhaustion is a Trap") | later |
| GenServer `init` | the child's init message is the first inbox row; `handle_continue` is `cx.send(self, msg)` | yes |
| GenServer `call` with timeout, caller sees callee death | `cx.call(target, msg, timeout_ms) -> call_ref`: one outbox row that sends `{"type":"call","ref":..,"from":self,..}`, installs a monitor keyed by the ref, and a timer. Reply arrives as `{"type":"reply","ref":..}`; callee death as `{"type":"down","ref":..}`; timeout as `{"type":"call_timeout","ref":..}`; the first of the three wins and the pump cancels the other two (`calls(ref, target, timer_ref)` table) | yes |
| GenServer `reply(from, msg)` | `cx.reply(from, ref, msg)` = a send with the ref | yes |
| GenServer `cast`, `handle_info` | `cx.send`; every message is `handle` | yes |
| GenServer `terminate(reason, state)` | `Behavior::terminate(&self, cx, reason) -> Result<()>`, default no-op; runs in a final transaction before status becomes `stopped`; not run for `kill`; a `Trap` inside it is recorded in `dead_letters` and ignored | yes |
| `code_change(old_vsn, state, extra)` | `code_changes` row + the new behavior's `schema()`; the behavior’s data migration hook runs in the promote transaction for data moves; a `Trap` there rolls the promote back | yes |
| module hot load affects every process | `node.promote_where(old_hash, new_hash, author, rationale)`: one promote per actor currently on `old_hash` (`_node.db` `who_runs(id, behavior_hash)` maintained by the pump); each is its own transaction; a failure stops the sweep and reports the actor id | yes |
| Supervisor `one_for_one`, `one_for_all`, `rest_for_one` | Addendum A | yes |
| `simple_one_for_one` / `DynamicSupervisor` | strategy `dynamic`: one child spec template, `start_child(init)` spawns a new child from it, `terminate_child(id)`; intensity as usual | yes |
| child spec `type: worker | supervisor`, `shutdown: brutal_kill | ms | infinity` | spec fields; on `shutdown`, a `supervisor`-typed child gets `infinity` by default; `ms` means: send `exit(shutdown)` (trappable), start a timer, on expiry `kill` | yes |
| `Supervisor.start_child/terminate_child/restart_child/delete_child/which_children/count_children` | messages the default `Supervisor` behavior handles; `which_children` and `count_children` reply to the sender | yes |
| `Application` (one root per node) | `node.root()` is a `Supervisor` with hash `supervisor-v1` created at node init; `spawn_root` children are its children; stopping the root stops everything by links | yes |
| `Task` / `Task.Supervisor` / `Agent` | behaviors written against the primitives above; not part of the crate | derivable |
| `spawn_opt(priority)` | no priorities; one message at a time per actor, actors scheduled fairly by tokio | deliberate |
| ETS (shared mutable tables) | no shared mutable tables; an ETS table is an actor; `_node.db` holds only names, groups and who_runs | deliberate |
| distribution (`{name, node}`, `monitor_node`) | ids reserve the cell prefix; `send` accepts any id; cross-node delivery and `monitor_node` are the cells lane (leaver: that lane's tests) | later |

### Tests 14 to 18 (goal becomes N/18)

14. `pair_fifo_and_down_after_messages`: A sends B ten messages then `exit("boom")`;
    B (monitoring A) holds the ten in order followed by exactly one `down`, also when
    the pump is restarted midway with half the outbox undelivered.
15. `defer_is_selective_receive`: B receives [x, y, z]; its behavior defers `x` until it
    has seen `z`; the commit order is y, z, x; a behavior that defers unconditionally
    traps after the second consecutive defer with an error naming the seq.
16. `call_reply_timeout_and_death`: A `call`s B (replies): A gets one `reply`, no
    `down`, no `call_timeout`, and the `calls` row is gone. A `call`s C (never
    replies, timeout 50 ms): A gets `call_timeout` only. A `call`s D (exits before
    replying): A gets `down` only.
17. `kill_terminate_and_shutdown_timeout`: `terminate` runs on `stop(id, "shutdown")`
    and writes a row; it does not run on `kill`; a trapping child with `shutdown: 30ms`
    that ignores `exit` is `killed` after the timer and observers see `killed`.
18. `dynamic_supervisor_and_registry`: a `dynamic` supervisor starts three children by
    `start_child`; `count_children` replies 3; `terminate_child` one; `whereis` of a
    registered child returns its id before and `None` after it stops; `members` of a
    group drops it too.

## MCP surface

The existing stdio and authenticated HTTP MCP endpoint shares one actor node with
Loom definitions. `loomd --actors-dir <path>` selects its directory; the default is
`<db parent>/actors`. Tool results are JSON text. Errors include actor and sequence
context when known. `init: null` spawns without an initial inbox message; other init
and message values are encoded as JSON bytes.

- `actor_list()` lists actor identities, status, behavior, cursor, inbox size, and parent.
- `actor_tree(root?)` returns a nested tree, defaulting to the node root supervisor.
- `actor_info(id)` returns lifecycle, mailbox, and relationship details.
- `actor_send(id, key?, msg)` sends a keyed message, runs until idle, and returns the cursor.
- `actor_spawn(behavior_hash, init, parent?, spec?)` spawns a child and returns its id.
- `actor_stop(id, reason)` stops an actor with the supplied reason.
- `actor_restart(id, verb)` applies `resume`, `skip`, or `reset`.
- `actor_promote(id, behavior_hash, author, rationale)` returns the new code-change row.
- `actor_promote_where(old_hash, new_hash, author, rationale)` returns promoted ids.
- `actor_lineage(id)` returns code-change rows.
- `actor_dead_letters(id)` returns failed-message rows.
- `actor_fork(id, at_seq)` returns a historical fork id.
- `actor_validate(id, candidate_hash, k, assertions?)` returns a verdict and SQL assertion results from the replayed candidate.
- `actor_sql(id, query, params?)` returns read-only query rows and refuses writes by statement kind.
- `actor_whereis(name)` resolves a registered name.
- `actor_register(name, id)` registers a unique name.
- `actor_members(group)` lists group members.
- `actor_behaviors()` lists registered hashes and descriptions.
- `actor_run()` runs until idle and returns the number of processed actor turns.

JSON resources are `actor://tree`, `actor://<id>/inbox`, `actor://<id>/effects`,
`actor://<id>/outbox`, and `actor://<id>/lineage`. Read tools and resources require
read scope; mutation tools require execute scope, except promotions, which require
define scope. A SQL assertion passes when it returns one nonzero numeric scalar.

## Addendum C: durability and failover

`Config.store` enables per-actor shipping through the `object_store` 0.14.1
`ObjectStore` trait. `None` retains local-only execution. `StoreConfig::Local`
selects a filesystem directory; `StoreConfig::S3` takes an endpoint, bucket and
region, with credentials loaded by `AmazonS3Builder::from_env`. HTTP endpoints
are enabled only when explicitly configured with `http://`, for local MinIO.

The upstream `LocalFileSystem` implements conditional creation but returns
`NotImplemented` for `PutMode::Update` (`object_store` 0.14.1 `local.rs:399`).
`local_store.rs` supplies that operation through the same upstream trait. An OS
per-object file lock covers version comparison and atomic publication, including completion
when the caller is cancelled. Filesystem publications use fsync. Every writer to
this local store must use this adapter; directly replacing its files bypasses
fencing. S3 uses `Create` / `If-None-Match: *` and `Update(UpdateVersion)` /
`If-Match`, preserving both returned version fields.

`ChildSpec.durability` defaults to `Local` and is persisted as the actor's
`durability` meta key at spawn. It is immutable: no setter exists, and reset
carries it forward. A Local message commits locally; the node ships at
`Config.ship_interval`, default one second, and on `Node::close`. A Remote message
publishes its prepared transaction's segment and conditional head before the
local transaction completes. If publication fails, SQL changes roll back and the
inbox message remains pending; storage failures do not poison it. Physical local
commit failure after remote acknowledgement is an uncertain outcome: reopening
recovers the published history. Short effects still require idempotency by their
existing actor/generation/sequence/index key.

The store layout is:

- `actors/<id>/snapshots/<epoch>-<seq>.db`: a complete `VACUUM INTO` snapshot.
- `actors/<id>/segments/<epoch>-<from_seq>-<to_seq>.bin`: canonical tagged JSON
  row additions and removals for inbox, effects, outbox, code changes and runtime
  bookkeeping. Integer values, blobs, nulls and floating-point bits remain exact.
- `actors/<id>/head`: JSON `{epoch, seq, snapshot_seq, snapshot, segments}`. Its cached
  version is the condition on every head update. `snapshot` holds the full object
  key (null before the initial snapshot), including the epoch that wrote it.
- `actors/<id>/lease`: JSON `{owner, epoch, expires_at}`. Expiry is Unix time in
  milliseconds; owner identities are generated per Node instance.

The durable revision `seq` is separate from the contiguous inbox cursor. Both
advance together for an ordinary message stream; a control change, selective
receive, failed publication reservation or snapshot can advance the durable
revision without advancing the cursor. This prevents two different states from
sharing an immutable segment key. The local `durability_seq` meta key is compared
with the remote head. Snapshot compaction bounds the active segment chain using
`snapshot_every`; reset publishes a new snapshot before replacing the live file.
Old object versions and unreferenced failed uploads are retained.

On first open, the node acquires a lease before serving the actor. A lease lasts
`Config.lease_ttl`, default ten seconds. Independent renewal workers renew at
TTL/3; a blocked handler or shipment does not block other actors' renewals.
`Config.lease_clock` accepts a `Clock` implementation for deterministic expiry
checks. Production clocks must be synchronized across owners: object storage
provides conditional writes, not a trusted shared clock.

Takeover conditionally replaces an expired lease with a higher epoch, then fences
head before restoring. An old head write already in flight may finish before
that fence; takeover reads that result. Once takeover finishes, the old cached
head version cannot publish. Every runtime commit checks the cached lease, with
no lease request on the message path. Conditional conflicts invalidate the cache.
An exact head read can recover an acknowledgement lost after a successful PUT;
a different head is never accepted as that acknowledgement.

Lease loss stops the actor with reason `lease_lost`, cancels its active handler,
and renames its database and WAL to `<id>.stale.<epoch>.db` and the corresponding
sidecar paths. Existing handles can inspect the stopped file. `Node::open` is the
leaver: after ownership can be acquired again it restores and serves a new live
file. Losing-owner bytes are not discarded. Ordinary supervision restart is not
the path out of `lease_lost`.

A missing or older local file is restored from the declared snapshot and the
complete segment range before its connection is admitted. A previous owner's
unshipped tail is archived when its epoch is superseded. A retained local file
from the immediately preceding owner is checkpointed before serving, preserving
its local tail. Restore replays with registered behavior hashes and recorded
short effects, never the external effect handler. Termination hooks and host
supervisor spawns have replay records; outbox delivery flags, pending inbox keys,
timers and other runtime rows are restored as well. Direct `Actor::sql` on a
store-managed actor is read-only; behavior SQL remains transactional.

Turso 0.7.2's public `turso::Connection` does not expose `wal_get_frame`,
`wal_changed_pages_after`, `wal_insert_frame` or `wal_insert_begin`; those methods
exist on `turso_core::Connection`. This implementation ships snapshots and logical
history through the existing replay path. WAL-frame shipping is the v2 replacement
for that unit, not an assumed API in this implementation. Delta construction
currently scans runtime tables; large histories therefore have a CPU and memory
cost even when the uploaded delta is small.

`Node::ship` flushes an actor and verifies its head fence. Background failures are
logged and available through `Node::shipping_failures` as the latest error per
actor. `Node::renew_leases` permits an explicit renewal tick. `Node::close` excludes
new admitted operations, flushes while renewal remains active, then closes and
releases leases. A flush failure leaves the node usable; release failures leave it
closed and a subsequent `close` retries release. Dropping a Node cancels its
workers but does not promise a final shipment; use `close` for that guarantee.

The five tests in `tests/durability.rs` cover fresh-node restore, Remote rollback
on failed puts versus Local success, stale-file preservation and head fencing,
takeover dedupe, and store-free behavior. Their assertions also cover competing
claims, expiry during a handler, renewal while a handler is blocked, termination
replay, and recovery from a failed close. They use local filesystem storage; an
S3/MinIO service is not part of this test suite.

## Addendum D: validation memo and cutoff

`validate` and `validate_assertions` consult `_node.db.validation_memo` before
creating a fork. Its columns are `key TEXT PRIMARY KEY`, `verdict BLOB`,
`tables BLOB`, `outbox_hash TEXT`, and `created_at INTEGER`. The verdict is
canonical JSON; `tables` contains the fork's named table hashes and assertion
results. A hit returns both without executing the candidate or copying a file.
Malformed persisted JSON is an error naming `validation_memo`.

The key uses BLAKE3 over length-delimited fields: candidate behavior hash,
BLAKE3 of snapshot file bytes, window boundaries, canonical inbox and effects
rows in `(N-k,N]`, and the ordered assertions list. Rows are ordered by their
primary keys; values have SQL type tags and byte lengths. Outbox hashes cover
`seq,idx,target,msg`, ordered by `seq,idx`; delivery bookkeeping is excluded.

The current replay engine needs more inputs than the proposed five-field key.
Snapshots can precede `N-k`, deferred messages use recorded commit order, and
data migration hooks consume effects at negative sequences. This implementation also
keys source metadata, code changes, the complete inbox/effect logs, and original
domain table hashes used to calculate the verdict. It hashes the latest snapshot
at or before `N-k`, not a synthesized snapshot exactly there. This conservative
key prevents stale hits but scans history; bounding those scans requires an exact
boundary snapshot and an explicit replay-input representation. That work is not
claimed here. Arbitrary SQL assertions and native behaviors must be deterministic
and obey the existing Behavior contract for memoization to be sound.

Under that contract, replay is a pure function of these inputs. External effects
come from the recorded log. `random()` derives bytes from actor identity, sequence,
and its per-message counter. `now()` currently reads a recorded effect keyed by
actor and sequence; it is not computed directly from that pair. Identity and
incarnation are included through snapshot bytes and source metadata. Runtime
failures return errors and do not become cached deterministic verdicts.

`promote_report(id, hash, k)` validates, checks that its input key still matches
while holding the actor lock, promotes, and returns `PromoteReport { verdict,
downstream_unaffected, receivers }`. The cutoff helper is
`history.rs::Node::promotion_cutoff`. Equal outbox hashes set
`downstream_unaffected` even when internal tables differ. Receivers are sorted,
distinct outbox targets in the original window. `ActorId` is currently a string
alias, so routing targets such as `effect:echo` are retained verbatim too. The report concerns
that historical window, not predictions about future messages. Ordinary
`Node::promote` still needs the call-site integration in `node.rs`; this scoped
change does not alter that file or invent a default window for its existing API.

`MemoConfig.max_rows` defaults to 10,000. `validate_with_memo_config` uses the same
validation implementation with an explicit limit. `memo::store` evicts oldest
records after insertion or a hit, inside one transaction; it is the leaver of
retained memo state. `created_at` is a monotonic insertion ordinal, so clock skew
and timestamp ties cannot reorder eviction. Zero retains nothing. Keys are never
invalidated; eviction affects performance only. Forks retain the existing history
API lifecycle and remain undeliverable; memo hits create no additional forks.

`lib.rs` re-exports `history::memo::{MemoConfig, PromoteReport}`.
`history.rs` owns `memo.rs` and includes `tests/memo.rs` as unit tests because the
crate manifest sets `autotests = false`. The four tests cover replay-free hits,
independent key changes, send cutoff despite state differences, and oldest-first
eviction. Existing tests remain in the integration target.

## Addendum E: capabilities

Actor identities locate files; authority is a host-minted `Cap`. This supersedes
the bare-id guest APIs above. A cap contains `target: ActorId`, `cap_id: u64`,
`epoch: u64`, `rights: Rights`, and `mac: [u8; 32]`. Rights occupy seven bits:
SEND, SPAWN, STOP, MONITOR, LINK, PROMOTE, and INSPECT. The MAC is
`blake3::keyed_hash(node_key, target || cap_id || epoch || rights)`, with numeric
fields encoded as eight little-endian bytes. The 32-byte key is generated at node
initialization and retained in `_node.db.meta`; actor files and messages never
receive it. A capability minted by another node therefore has no authority here.

`cx.send(&cap, msg)`, `stop`, `shutdown`, `monitor`, `link`, `unlink`, `restart`,
`inspect`, `inspect_sql`, `promote`, `send_after`, `call`, and `reply` take capabilities. The host
checks the MAC, rights, the target's `meta.capability_epoch`, and its
`revoked(cap_id)` table. Invalid authority is a deterministic `Trap` naming the
operation and cap id, even when the behavior ignores the returned error. It is
not retried as an environmental failure. Timer and monitor references remain
handles for cancelling operations already owned by the calling actor. Shutdown
policy lives in the target's metadata, so a STOP holder can request it without
being the parent; the requester's pump clears its completion barrier.

`cx.spawn(&spec)` returns a full-rights child cap and stores it in the parent's
`caps(cap_id PRIMARY KEY, target, epoch, rights, mac)` table. The spawn outbox owns
the pending child until the pump creates its file; its cap can authorize a send
in the same transaction. `cx.self_cap()` grants authority over the caller itself.
`cx.cap(cap_id)` retrieves a held token. `cx.attenuate(&cap, subset)` verifies the
source, derives a distinct token with no additional rights, re-MACs it, and stores
the result. Serialized caps can travel in message payloads; `cx.accept(cap)`
verifies and stores a received token before a later turn retrieves it.

`cx.revoke(cap_id)` verifies a held token and queues its revocation for the pump,
which inserts that id in the target's `revoked` table after the caller commits.
Later operations in that transaction already refuse the revoked token; if the
transaction traps, its revocation rolls back with its other writes.
Revoking one token leaves independently minted tokens valid. `node.bump_epoch(id)`
increments the target's epoch and invalidates all earlier tokens for it. A fresh
operator cap uses the new epoch. Reset preserves held caps, revocations, and the
capability epoch while replacing domain state; reopening a node retains its key.

Capability operations use the recorded `__cap` effect boundary. Validation's
intercepting handler matches their requests against history and returns recorded
results without invoking live verification, minting, inspection, or revocation.
Forks retain their caps as live tokens, so isolation rests on this interception
and the pump's refusal to deliver fork outboxes. An unchanged candidate can still
match after authority has changed in the live node. Public `effect` and `request`
reject reserved host operation names.

The root supervisor persists child caps in its `spec` table. Existing supervisor
specs and host spawn journals are migrated when opened. MCP callers remain the
operator: tools mint the required authority through `node.cap_for(id, rights)`
and verify it before their host action. `Node` and `Actor` remain host interfaces;
guests receive `Ctx`, never the operator minting method.

Behavior SQL is parsed before execution. Guest writes may target domain tables,
but cannot mutate runtime tables, attach databases, control transactions, issue
pragmas, install triggers or views, or invoke filesystem/extension functions.
Runtime supervision uses a crate-private SQL entrypoint. Read-only inspection
also rejects functions with external or mutating effects. INSPECT permits reading
the actor file, including bearer tokens in caps, messages, and domain data; hand
it out only when that disclosure is intended. These checks protect
the `Ctx` boundary; native Rust behaviors and their installed schemas are trusted
host code, not a process or filesystem sandbox. A future wasm guest must expose
only this checked boundary.

The six properties in `tests/caps.rs` cover forged tokens, attenuation,
delegation, individual and epoch revocation, reset and reopen persistence, and
fork interception across capability operations. Together with the prior 32
actor tests, the requested end state is 38 passing actor tests. The combined
integration command is `cargo test -p loom-actor -p loom-mcp`.


The `loom-behavior` bridge carries a capability as serialized UTF-8 JSON token
bytes in the `cap` field of each Loom CBOR `actor.*` descriptor. Spawn and
attenuation return the same bytes. A guest receives tokens in message payloads,
calls `actor.accept` to verify and persist them, and can retrieve accepted tokens
with `actor.cap` using a decimal-string `cap_id`; `actor.revoke` uses that same
string representation to preserve all 64 bits through Loom descriptors. The SDK's `actor::Cap` is a named wrapper over those
bytes; `actor::send` accepts that handle, never an actor ID. The bridge decodes
tokens and invokes the existing cap-taking `Ctx` methods, so native and wasm
guests share MAC, epoch, rights, revocation, and replay checks. Neither a sender
ID nor a descriptor naming a target grants authority. Invalid tokens produce a
deterministic dead letter naming the operation and `cap_id`, even when the guest
ignores the returned error. `crates/loom-behavior/README.md` defines the complete
descriptor contract; `guest_without_cap_cannot_send` exercises a forged token
through a compiled guest and checks rollback and absence of delivery.
