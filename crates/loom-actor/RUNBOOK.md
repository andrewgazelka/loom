# loom-actor

From the repository root:

```sh
cargo fmt -p loom-actor
cargo check -p loom-actor
cargo test -p loom-actor --no-fail-fast
cargo clippy -p loom-actor --all-targets -- -D warnings
```

One integration target contains 22 `#[tokio::test]` functions, including `memory_io_is_wired`. The user lifted the write-only restriction for this gate. Local execution on 2026-09-11 after MCP integration produced:

```text
test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.19s
clippy exit code: 0
```

`cargo check` and `cargo fmt` exited 0. The workspace emits a Cargo configuration warning about `build.analysis` requiring `-Zbuild-analysis`; no workspace configuration was changed. No commits were made.

## Specification choices and deviations

- `code_changes.seq` is a monotonic revision starting at 0; `meta.code_at:<message_seq>` records the committed revision, preserving multiple promotions before any message succeeds without changing the specified tables.
- A cursor-0 snapshot supplements periodic snapshots so `fork(id, 0)` and validation windows preceding the first periodic snapshot are supported.
- `Ctx::sql` returns buffered `Rows { columns, rows }`, because Turso executes while polling and dropping an unconsumed write result must not silently omit the write.
- Verdict table entries are named `TableHash` and `TableDifference` structs instead of anonymous tuples, as required by the supplied AGENTS.md; a missing table's hash is the explicit string `absent`.
- `meta.skipped:<seq>` records supervision/manual skips so historical replay preserves skipped messages; forks also retain dead-letter audit rows through the requested sequence.
- `meta.replay_source` preserves deterministic random/child identity during replay and prevents a stopped fork from delivering; `promote` preserves fork status, and `stop` is its terminal transition.
- Runtime notifications append after existing outbox positions so lifecycle signals follow earlier sends; hook effects use a separate negative sequence namespace.
- Child derivation encodes seq and idx as fixed-width little-endian i64 values; the `a0` prefix precedes the 26-character ULID or truncated child hash, and reset generations use the incarnation namespace described below.
- Effect divergence `expected`/`got` bytes encode JSON `{kind,request}` signatures; an absent call is an empty byte vector, and replay detects both added and omitted effects.
- The default handler additionally implements `now` as little-endian i64 Unix milliseconds; `alarm` accepts a JSON u64 delay in milliseconds, sleeps, then echoes those request bytes.
- Management errors use seq -1 when no inbox message is selected; message execution and delivery errors include the actual actor id and sequence in their error chain.
- Receiver assertions in tests 3 and 4 exclude the mandatory root-init mailbox baseline instead of deleting append-only inbox rows; test 3 separately asserts no message from A exists.

Schemas run once per behavior hash per file, including during replay; promoting an earlier hash appends lineage without rerunning its original schema. Native behaviors are trusted to mutate domain tables only and to leave transaction control and runtime tables to the runtime. `Actor::sql` is the host inspection/administration interface.

Creation and fork publication use staged databases and `VACUUM INTO` before rename, so a published actor does not depend on a staging WAL. Snapshot registration follows publication and is retried at the current cursor before another message runs. Forks share immutable snapshot paths with their source. A Node owns its directory's connection cache; do not operate separate live Nodes on the same directory concurrently.

Long-effect handlers receive the same durable key on redelivery. Delivery errors return from `run_until_idle` with the row still pending; calling it again retries delivery. They cannot roll back an already committed handler transaction. Custom handlers must implement every external short-effect kind their behaviors use, including `now` when used; the runtime owns the recorded child-inspection effect and has no fallback for other kinds.

`validate_assertions` evaluates explicitly supplied read-only SQL against the candidate replay, returning the validation verdict and a pass flag per assertion. Assertions pass only for a single numeric, nonzero scalar. `Actor::inspect_sql` parses one SELECT (or EXPLAIN SELECT) before execution and refuses write statement kinds and multiple statements.

## Turso 0.7.2 source observations

- `Statement::query` binds parameters and returns without stepping; fully drain every query, including pragmas and writes issued through `Ctx::sql`.
- `Statement::execute` rejects a row-producing statement with `Misuse("unexpected row during execution")`; all SELECTs and pragmas use `query` and drain.
- The usable transaction is `turso::transaction::Transaction<'conn>`, returned by `Connection::transaction()`; the top-level `turso::Transaction` is an empty placeholder.
- Dropping the usable transaction schedules rollback on the original connection's next operation; the handler-error path explicitly awaits `rollback`, while propagation of other transaction errors uses that documented deferred rollback.
- `execute_batch` internally executes each statement rather than draining query rows; behavior schemas must consist of non-row-producing DDL.
- `Rows` and `Row` expose column counts, and `Row::get_value` supports explicit typed hashing; rows are drained before another statement or commit.
- `Builder::experimental_vacuum(true)` is enabled for every file connection, including staging and replay databases.
- The public transaction source contains an old comment about missing savepoints, but `turso_core-0.7.2/translate/rollback.rs` implements SQL SAVEPOINT, RELEASE, and ROLLBACK TO; termination hooks use those SQL operations inside the final transaction. The successful terminate-hook path is exercised by `kill_terminate_and_shutdown_timeout`.

## Exact test names

1. `three_messages_three_rows`
2. `crash_before_commit_reruns_once`
3. `trap_rolls_back_send`
4. `send_delivers_exactly_once`
5. `short_effect_keyed_and_recorded`
6. `long_effect_result_arrives_as_message`
7. `fork_matched`
8. `fork_diverged`
9. `promote_then_rollback_lineage`
10. `monitor_delivers_down_once`
11. `link_cascades_stop`
12. `one_for_one_reset_then_intensity`
13. `rest_for_one_order`
14. `pair_fifo_and_down_after_messages`
15. `defer_is_selective_receive`
16. `call_reply_timeout_and_death`
17. `kill_terminate_and_shutdown_timeout`
18. `dynamic_supervisor_and_registry`
19. `validate_differs_and_multi_promotion`
20. `resume_keeps_tree_intact`
21. `shutdown_wait_does_not_stall_node`
22. `memory_io_is_wired`


## Addendum A integration choices and deviations

- `Ctx::spawn(&ChildSpec)` replaces the original two-argument API; the constructor takes the registry-resolved `Behavior::child_type()`. Constructor and JSON defaults select permanent restart and `link=true`, with infinite shutdown for supervisors and brutal shutdown for workers. An explicit `shutdown` overrides that default; behavior hashes do not determine child type.
- `Node::stop(id, reason)` replaces the reasonless API; stopped live actors now leave through explicit `restart`, while replay forks remain ineligible for live restart.
- Spawn rows use a tagged `child` or `restart` payload, keeping all creation and resume/skip/reset operations on the specified `spawn` target instead of introducing parallel restart targets.
- Runtime delivery adds `down:<watcher>` and `exit:<peer>` targets so monitor retirement and automatic exit propagation are durable pump operations, not nontransactional post-stop callbacks.
- Lifecycle messages include generation, event identity, and optional initiator; shared event identity deduplicates poison/down/exit, and initiator distinguishes intentional group shutdowns from new failures.
- `EffectKey` includes `generation`; generation zero retains the original delivery/child namespace, while later incarnations use `<id>@<generation>` so reset cannot deduplicate new work as an old call.
- Fresh-state reset archives a WAL-complete `VACUUM INTO` copy instead of literally renaming a potentially incomplete main database; a fully built replacement is published with a recoverable marker, preserving the same actor ID and shared connection slot.
- Reset reapplies each distinct historical schema before retaining the original code-change head and lineage, because an upgraded behavior's ALTER schema alone cannot create its base tables.
- Applied-control receipts survive reset with active links and monitors, preventing redelivery of old stops, monitor actions, or reset commands from affecting the new incarnation; consumed monitors retain a receipt and fire only once.
- Reset generations get separate snapshot paths; generation zero retains `<id>.snap.<seq>.db`, so archived snapshots are not overwritten by later incarnations.
- `Supervisor` is registered by default under `supervisor-v1`; its initial JSON message is `{"type":"configure",...}`, with omitted fields retaining one_for_one and 3 restarts in 5 seconds.
- Supervisor configuration fields are `strategy`, `max_restarts`, and `max_seconds`; restart intensity counts one strategy round per initiating child, not one entry for every sibling in that round.
- Child inspection goes through `Ctx::inspect` as a recorded runtime effect backed by `node.open`, preserving deterministic supervisor replay instead of giving the behavior an unrecorded Node reference.
- The newer-code test compares the current code revision to the revision saved when poison occurred, rather than comparing incomparable code revisions and inbox sequence numbers.
- Supervisor children are monitored as well as optionally linked, and monitors are rearmed after reset so normal exits still apply permanent/transient/temporary policy.
- Selected temporary siblings stop without restarting, honoring the explicit `temporary: never` rule when a group strategy selects them.
- `Shutdown::TimeoutMs(n)` serializes as `{"timeout_ms":n}`; graceful shutdown delivers a trappable exit and persists a kill deadline. `Infinity` has no deadline, and `Brutal` bypasses termination hooks.
- Native handlers must yield for cooperative cancellation to take effect; v1 does not run native code in a separately killable process.
- `tree` returns named `TreeEntry` records in breadth-first order, with siblings in spawn order; names are persisted in `_node.db`, and registering an occupied name fails clearly.
- History and reset logic moved into `history.rs` and `reset.rs`, and lifecycle operations into `supervision.rs`, keeping every Rust source file below 400 lines while `supervisor.rs` remains the normal behavior implementation.

The supervision tests also cover both trap-exit modes, duplicate stop delivery, name removal on stop/reset, and ordered tree inspection. The final gate above supersedes the earlier write-only source review.


## Addendum B integration choices and deviations

- `Node::new` is async because initialization now creates or reopens the durable root Supervisor; `root()` returns its ID. The root marker is actor metadata, not a fourth node-level table.
- `spawn_root` registers linked temporary children under the node root: the external spawn API supplies no restart policy, so an explicit supervisor child spec selects automatic restart behavior.
- Inbox rows gain `state`, `defer_epoch`, and `defer_count`; metadata tracks committed execution order and contiguous historical boundaries. Two deferrals without an intervening commit trap.
- Forking at a sequence which never represented a complete contiguous mailbox boundary fails explicitly; historical execution order is retained for selective-receive replay. A candidate which defers a historically committed message returns `Trapped` rather than inventing a new replay schedule.
- `timers` persists target, payload, deadline, kind, arming state, and initiator; timer alarm rows arm the node scheduler without sleeping inside delivery. Timer scanning runs independently of actor turns; cached shutdown deadlines can cancel a yielding but unfinished handler.
- Calls persist first-winner receipts in metadata after removing their active rows, so redelivered replies, DOWNs, and timeouts cannot produce a second outcome.
- `shutdown:<id>` is the pump operation for child-spec graceful shutdown; `stop:<id>` remains the explicit stop primitive. Native code must yield for cooperative handler cancellation to take effect.
- Lifecycle hooks use negative logical sequence numbers for recorded effects and dead letters, leaving inbox sequence numbers unchanged; deferring from a hook is a Trap because it has no current inbox message.
- Outbox positions are monotonic in execution order: after selective receive or runtime notifications, a physical outbox position may differ from the logical inbox/effect position; `request` returns the physical result key. This preserves FIFO without changing effect identities.
- Stopping an actor cancels its owned timers, preventing timer sends after its final lifecycle signals.
- Only the node root uses a ULID; `spawn_root` actors are now children and use the same derived IDs and durable spawn path as behavior-created children.
- `ChildType` is serialized as the spec field `type`; an omitted shutdown selects infinity for supervisor children and brutal for workers.
- `promote_where` synchronizes the derived `who_runs` index before selecting actors and applies independent actor transactions, stopping on the first error.
- Runtime scheduling, lifecycle hooks, mailbox selection, and node-directory operations have separate modules to keep every source file below 400 lines.
- `meta.ready` gates newly created children until the spawn row has installed links and monitors, including recovery after interrupted publication.
- Group restart delivery waits while any owned child has a pending graceful shutdown, ensuring the selected children stop before fresh generations start.
- `memory_max` and `fuel` are initialized metadata only; enforcement belongs to the future wasm lane, as specified.

## Executed gate integration fixes

- `directory.rs` now names `anyhow::Error` and collection result types explicitly where Rust could not infer them.
- Test 3 asserts that the application send rolls back while the required poison notification reaches the new root supervisor; it no longer assumes a parentless actor with an entirely empty outbox.
- `autotests=false` and `tests/integration.rs` combine the three property modules into one 21-test target, preserving every named test and producing the requested single result line.
- `types.rs` and `supervisor_store.rs` hold public data types and child-spec persistence; crate-root API reexports remain unchanged.
- Crate-local `rustfmt.toml` uses a 140-column width and maximum small-item heuristics; all Rust files remain below 400 lines after `cargo fmt`.
- Clippy findings were fixed with derived defaults, explicit eager defaults, let chains, and iterator enumeration; no lint suppressions were added.


## Review fixer regressions

- `EffectHandler::call` returns `EffectError::Environmental` or `EffectError::Deterministic`; deterministic errors immediately trap, including when a behavior ignores the returned error. Environmental errors retain bounded retries.
- Poison parks the actor and notifies its parent; monitor DOWN is emitted only on an actual stop. This supersedes Addendum A's poison-as-DOWN rule to satisfy `resume_keeps_tree_intact`.
- Resume preserves links and monitors and does not consume restart intensity; Reset alone shuts down a selected child before restarting it.
- `shutdowns(child,request)` in each parent's file records pending child shutdowns explicitly; a busy connection alone is not evidence of shutdown.
- `validate_differs_and_multi_promotion` reconstructs both sides of the schema promotion boundary and verifies domain-table differences with identical effects.

Mutation control actually executed for test 19:

```sh
cargo test -p loom-actor validate_differs_and_multi_promotion -- --nocapture
```

The comparison `original.get(name) != replayed.get(name)` was temporarily disabled using `false && ...`. The mutation was then restored; no mutation remains in the source.

```text
original: test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.17s
mutated: test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.17s
mutation Cargo exit code: 101
restored: test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.16s
```

The mutation failed at the assertion expecting `Differs`, with an incorrect `Matched` verdict, rather than at compilation or setup.

- The node-wide delivery guard is replaced by per-sender pump guards and per-actor lifecycle/relationship guards; choosing a guard never retains the map lock while awaiting actor I/O or timers.
- Each scheduler task drains at most `Config.batch_limit` (default 64) messages under the actor connection lock, with a separate transaction per message and one pump after the batch. Empty inbox, cancellation, deferral and poison end the batch; budget exhaustion re-wakes the actor. Deferral re-enters admission so selective receive and the repeated-deferral trap retain their ordering. The batch reports its processed count and continuation directly; there is no SQL work probe.
- The scheduler admits actor tasks only from the shared actor-ID wake set. Delivery, publication, lifecycle changes, mutating host SQL and timer firing name their owner; a wake received during a turn remains pending until that task completes. Recovery seeds the set once. Progress never invalidates every idle actor.
- Timer scans run on recovery, arming or the earliest armed deadline. A scanner that encounters an active handler records its owner for a rescan on completion; cached shutdown deadlines remain available to cancel that handler. There is no 5 ms poll.
- Shutdown completion wakes its recorded requester pumps. Request creation and target completion dirty only their requester; clean pumps skip the shutdown query. Lifecycle/code changes dirty the node index; clean pumps skip index reads and writes, and dirty syncs compare observations under the actor lock.
- `run_until_idle` drains runnable inboxes, deliverable outboxes and due timers through the wake set, and awaits future armed timers. Parked/deferred/fork/unpublished work retains its lifecycle admission rules. Unknown wake IDs fail actor opening instead of disappearing.
- `terminate_child` emits shutdown without first inspecting a busy child; restart/delete retain their state checks.
- `shutdown_wait_does_not_stall_node` waits for a real durable shutdown request against a child holding its transaction, then asserts Y commits and advances its cursor within 50 ms and before X's 300 ms kill.
- Restart publication receipts preserve `ready=false` across interruption until monitor installation finishes, without a node-wide publication lock.
- Lifecycle scheduling moved into `scheduler.rs` and `lifecycle.rs` to retain the 400-line limit after formatting.

- Shutdown deadline cache insertion/removal is serialized with the actor connection and lifecycle transition; callbacks check the cache before signalling cancellation, so a stale deadline cannot cancel a replacement generation.

## MCP runtime

Every node registers `counter-v1` (records messages), `forwarder-v1` (sends `forwarded` to the actor ID in the message), `echo-v1` (replies to call envelopes), and `supervisor-v1`. Additional registered native behaviors remain available through `Node::behaviors`. Empty initialization bytes mean no initial inbox message; reset preserves that choice. Nonempty initialization is the first inbox row.

`Node::spawn(parent, spec)` persists the child specification and spawn outbox before delivery. `actor_ids` includes forks; `run_until_idle` returns the number of actor turns processed, excluding outbox-only delivery and timer scans. MCP shares the daemon's node and existing authenticated HTTP or stdio endpoint. Use `--actors-dir` to choose its storage directory.

`Config.io` selects `Auto`, `Syscall`, `IoUring`, or `Memory` through Turso's named VFS. Auto selects io_uring on Linux (the target dependency unifies Turso's feature), syscall elsewhere. An explicit io_uring request fails with the platform name on other systems. Memory keeps actor and directory-index databases in live connections and creates no database files; it has no disk snapshots or persistence across node lifetimes. Historical fork/validation and reset currently require persistent I/O. The `memory_io_is_wired` test exercises three messages and checks that the directory remains empty.

Kill and shutdown deadlines cancel only the native handler future, then await the message transaction rollback. They do not abort BEGIN, COMMIT, snapshot publication, or lifecycle transactions. Turso 0.7.2 marks `Transaction::in_progress` false only after awaited COMMIT returns; dropping the task between the engine commit and that assignment queues an invalid rollback on the next connection access. The io_uring parity run exposed this completion window; the corrected cooperative cancellation path passed the full 22-test actor suite on Linux with Auto selecting io_uring, as well as the local macOS suite.

With an explicit syscall/io_uring VFS, Turso treats `:memory:` as a literal filename. Scratch connections used while replacing a cached connection therefore select `Io::Memory` explicitly, independently of the node’s persistent I/O selection. The reset regression passes without creating `:memory:` or `:memory:-wal` files.

On Linux hosts with an 8 MiB locked-memory limit, concurrent io_uring-backed tests can fail with `io_uring_setup: out of memory` despite ample ordinary RAM. The 22 actor tests and 4 MCP tests pass with `RUST_TEST_THREADS=4 cargo test -p loom-actor -p loom-mcp` on such a host. Size test concurrency against ring-allocation headroom; keep the selected VFS unchanged.

## Drivers

Drivers own resources outside actor transactions. Implement the native `Driver`
trait and register it by overriding `Registry::resolve_driver(hash)`. The default
resolver refuses unknown hashes. `Driver::hash()` must match the requested hash;
`Ctx::spawn_driver(def_hash, init)` validates this through the recorded capability
boundary, so an unknown hash traps the spawning message even if its handler
ignores the error. Registry resolution must only look up code; it must not open
resources. Replay uses the recorded resolution and capability result.

`cx.spawn_driver(hash, init).await?` returns a root `Cap` with SEND and STOP rights
and writes a spawn outbox row. The existing pump opens the resource after commit,
through its single `drv:` arm. Spawn and message delivery share the ordinary
per-destination FIFO rule. No TCP-specific code runs in the kernel.

The two communication operations are:

- `DriverContext::inject(cap, key, bytes)` inserts a keyed inbox row into the actor
  named by a SEND capability. Sender is `drv:<driver id>:<handle>`. Duplicate keys
  retain one inbox row, including across resource reopenings. `owner()` supplies
  the owner's SEND capability; `for_handle(name)` only scopes the sender string.
  Injection also stores a SEND capability for that sender in the receiving actor's
  `caps` table. Live and replay mailbox decoding preserve driver senders. The
  handler obtains the recorded capability with `cx.sender_cap().await?` and replies
  with the existing `cx.send(&cap, bytes)`.
- The pump delivers a `DriverDelivery { handle, key, bytes, .. }` to the receiver
  passed to `Driver::run`. Call `delivery.acknowledge(Ok(DriverAck::Delivered))`
  after applying it, or `Dropped` when the handle has gone. An error leaves the
  outbox row pending and blocks only later rows for that same destination. A lost
  acknowledgment is an error. Drivers must deduplicate the delivery key before
  repeating resource I/O. The acknowledgment is the return half of delivery,
  not a separate actor operation.

Driver IDs have the form `<owner actor id>/<derived child id>`; targets are
`drv:<driver id>:<handle>`. `root` is the driver's control handle. The derivation
includes the owner's incarnation and spawn effect position. Driver capabilities
use their owner's existing capability epoch and `revoked` table. Attenuation and
revocation use the normal capability operations; no driver authority database
exists. A closed handle's valid cap remains sendable so the pump can record its
drop. The root cap can be stopped with `cx.stop(&cap, reason)` after commit.

A `Dropped` acknowledgment, or delivery to an absent driver, creates a terminal
`meta` entry in the sending actor named **`driver_drop:<delivery key>`**, whose value
is the destination. The ordinary pump then sets `outbox.delivered=1`. The marker
is an audit fact, has no retry transition, and stays with the owner's history.
The outbox retains the original payload. A retry between marker commit and the
normal delivered update is safe.

Drivers are linked to their spawning owner. Owner stop, poison with stop policy,
reset, and node close cancel them; dropping the last Node handle aborts them too.
`Driver::run` must own every resource and child future and release them when
cancelled. It must not detach resource tasks. Drivers use a private injection
channel: aborting resource code cannot abort the kernel's admitted transaction or
COMMIT. `run` returning an error or panicking closes the driver and injects one
`{"type":"down","ref":"spawn:drv:<id>:root","from":"drv:<id>:root",...}` into
its owner, with reason, event, generation, and initiator fields like actor DOWNs.
Normal return emits DOWN with reason `normal`. Deliberate owner/node cancellation
suppresses DOWN. Failed DOWN insertion retries while the node remains open.

The owning actor's persisted policy decides whether to call `spawn_driver` again,
using initialization assembled from its tables. The kernel does not invent a
restart policy or retry a failed resource under its previous identity. A built-in
Supervisor can restart the owning actor according to that actor's child spec;
the owner's initialization then reopens its resources. Driver roots themselves
are not entries in the built-in Supervisor's actor-only `children`/`spec` tables.

Driver resources, sockets, handle maps, and delivery receipts are **not durable
and never shipped**. The owner's tables are the source of truth. Node restart
does not restore drivers automatically. A `driver_spawn:<id>` metadata receipt
prevents an already-attempted spawn from opening again, including if the node died
before the pump set its delivered bit. The owner's next message or supervisor
restart must issue a new spawn. Pending sends to old handles are dropped. The
receipt records an attempt, not proof that the resource is currently live;
`driver_down:<id>` records a completed driver's reason. No DOWN is synthesized
for a node crash. Resource-derived ingress keys must remain unique in the target
inbox; the kernel does not prepend a driver ID to the caller's key.

### TCP listener

Register `drivers::tcp::TcpListenerDriver` under `drivers::tcp::HASH`
(`tcp-listener-v1`), then spawn with `{"bind":"127.0.0.1:0"}`. Its first injection
is `{"type":"listening","addr":"127.0.0.1:<bound port>"}`, with key
`driver:<id>:listening`. Accepted sockets have unique ULID handles, and each frame
is `u32` big-endian length followed by raw bytes. Ingress keys are
`conn:<connection id>:<frame number>`, starting at 1. A newly accepted socket is a
new resource even after restart; its ID is never reused. For drivers that reopen
an existing resource, derive stable keys from that resource's own offsets or IDs.

The TCP implementation limits frames to 16 MiB and has bounded delivery queues.
It keeps the partial read future alive across outgoing sends. Outgoing frames
are deduplicated by delivery key for the socket's lifetime; closing the socket
makes all later sends terminal drops. EOF, framing failure, and write failure
retire only that handle. The owner receives `{"type":"closed","handle":...,"reason":...}`
with key `conn:<id>:closed` after the socket halves are dropped. A write error or
five-second write timeout closes the stream because a partially written frame
cannot safely be retried. A successful write means acceptance by the local socket,
not an application-level receipt from the peer. Whole-driver failure reports DOWN.

The `drivers` test target covers keyed ingress, commit-only replies, delivery
deduplication, closed-handle drops with another live destination, owner-stop
cleanup, unknown-hash refusal, reopening from owner state, capability revocation,
and the node-restart spawn window. These tests were
written in a write-only lane; the coordinator runs the integration gate.

## Turn statement floor (write-only change, coordinator gate pending)

- Turn admission reads status, ready, generation, cursor, code and commit epoch once per batch. The connection lock is the cache lifetime: dropping it invalidates every observation. Initialize, reset, promote, lifecycle stop/restart, poison, replay/archive, publication and host SQL must acquire that same lock; none can change admission or code between messages in a batch. Successful completion alone advances the local epoch. No persistent Actor cache or invalidation counter is needed.
- Mailbox completion updates the inbox row, seeks the first pending and deferred seq separately, and uses the smaller seq minus one as cursor. With no open rows it seeks the last inbox seq, defaulting to zero for an empty inbox. An indexed existence probe for done rows above cursor supplies the boundary predicate. Epoch, commit order, cursor, optional boundary and code revision share one metadata write. Epoch overflow is checked before any completion write; cursor underflow fails the transaction. There is no additional cache. The committed result supplies send outcomes and snapshot admission; each qualifying cursor is snapshotted before the next message, including inside a batch.
- Pumps read metadata and pending outbox rows once per batch. The metadata read includes persisted publication-receipt existence for recovery. Only a persisted receipt or a restart acknowledgment in this pump enables the publication-row query. Host SQL mutations wake a fresh pump. Receipt writes/deletes and child readiness still commit through control transactions.

For a local memory no-op, with no timer, publication, dirty index or shutdown work, the previous path issued **28 SQL statements per message**: admission status/ready/generation/cursor (4), mailbox epoch/selection (2), code (1), BEGIN (1), attempt generation (1), completion inbox update/epoch read/epoch write/order write/cursor aggregate/cursor write/boundary probe/boundary write (8), code-at write (1), COMMIT (1), outcome cursor/state and final status (3), pump status/replay-source/generation/outbox/publications (5), scheduler probe (1).

The indexed path issues **9 statements per pending message**, plus one last-inbox seek on the final completion: two mailbox selections, BEGIN, inbox update, two first-open seeks, boundary probe, combined metadata write, COMMIT. Completion itself uses five statements, or six with no open rows. Each batch adds 6 admission reads and 2 pump reads; discovering an empty inbox adds two selections. A preloaded 1,000-message no-op therefore has 9,131 statements across 16 batches, excluding setup and external injection. Handler SQL/effects, remote shipping, outbox delivery acknowledgments, timer checks, lifecycle work and disk snapshots add their own statements. These are source counts, not timings. The former six-statement path performed a full completion aggregate each turn; fewer statements did not mean less work.

Mechanism coverage in `tests/scheduler.rs` adds 100 exact completions in two to three scheduler tasks, and a mid-batch deferral which ends its task, commits the releasing message, then retries the deferred message in selective-receive order. No builds or tests were run in the write-only lane.

The scan lane adds task-local statement counting at depths 2 and 2,000, native EXPLAIN assertions for ordered index searches, a missing-index negative control, aggregate-equivalence checks with deferred holes, mixed-epoch selection equivalence, outbox ordering after 2,000 delivered rows, and reopening a file without the indexes. The coordinator must run these fixtures and the existing three-message/defer gates; this lane did not run Cargo or Nix.

### Inbox and outbox index evidence and remaining work

`schema.rs` defines `(state,seq)`, `(state,defer_epoch,seq)`, and `(delivered,seq,idx)` indexes once. `SCHEMA` includes their `CREATE INDEX IF NOT EXISTS` statements for `actor/initialize.rs` and `reset.rs`. Forks in `history.rs` and remote snapshot restoration in `shipping_history.rs` copy files instead of rerunning SCHEMA. `actor::connect` applies the same index statements when an inbox table already exists, covering those copies and ordinary reopen. Index creation errors propagate. Queries name their indexes with `INDEXED BY`, so a missing index cannot silently restore a scan. Table rows and columns are unchanged.

Source evidence in the installed `turso_core-0.7.2/translate/`:

- `optimizer/mod.rs:663` recognizes a single unwrapped MIN/MAX aggregate; `:699` explicitly permits WHERE predicates. `:790` disables that optimization unless the chosen access path supplies the extremum order. The former combined MIN/MAX/COUNT expression cannot qualify.
- `optimizer/order.rs:786` handles B-tree ordering; `:840` skips equality-constrained leading columns before matching the ORDER BY suffix. Thus `state='pending' ORDER BY seq LIMIT 1` can use `(state,seq)` directly. A range on defer_epoch cannot be skipped this way.
- `main_loop/body.rs:229` handles simple MIN/MAX; `:272` jumps to the loop end after the first qualifying non-null extremum. `result_row.rs:390` emits LIMIT's `DecrJumpZero`. The implementation uses ordered LIMIT seeks rather than nullable aggregates; tests also ask the native planner about separate MIN/MAX queries.

Completion reads are `SELECT seq ... WHERE state='pending' ORDER BY seq LIMIT 1`, the equivalent deferred seek, optional `SELECT seq FROM inbox ORDER BY seq DESC LIMIT 1`, and `SELECT 1 ... WHERE state='done' AND seq>? LIMIT 1`. `refresh_cursor` shares these first-open/last-row queries. Their work is O(log N) regardless of completed history size. The pump reads `SELECT seq,idx,target,msg ... WHERE delivered=0 ORDER BY seq,idx` through the delivered index, costing O(log N + U) for U undelivered rows; delivering U outputs necessarily costs at least O(U).

Selection first seeks the smallest deferred seq. If its epoch is old, it is the exact winner and only one query is needed. With no deferred rows, one pending seek suffices after that probe. Otherwise selection queries the eligible epoch range ordered by seq, then pending, then reuses the first deferred row. This preserves the old CASE ordering, including arbitrary host-written epoch layouts. **The mixed-epoch branch still sorts eligible deferred rows and is not O(log N). The full requested complexity goal remains unfinished.** A runtime-generated mailbox has current-epoch deferred rows as a seq-prefix of all deferred rows: deferral consumes the first eligible row, and a commit advances the epoch. That would permit a last-current-epoch seek followed by a seq seek, but `Actor::sql` supports writes that can violate the invariant. Enforcing it at mutation/restore boundaries or maintaining an augmented range-min index requires an explicit design choice; this patch does not silently change ordering for those files.

Adjacent costs remain: publication-marker `EXISTS ... key LIKE 'publish:%'` is not addressed here; each delivery reads source generation and uses receiver and acknowledgment transactions. Capability authorization and outbox-position reads remain per send.

### Completion regression follow-up

The coordinator's gate for integration commit `3fa9c5c` failed five targets; the integration target had 1 pass and 26 failures. Fresh-message domain writes were absent and actors were parked. Source tracing finds live admission reads, committed publication wakes and a root wake during Node initialization. Node initialization enqueues configure; it does not itself drain that message before host spawn.

The new completion SQL is the remaining source-level suspect: a completion error is a runtime Trap, so retries roll back domain writes and exhausted retries poison/park the actor. Scheduler progress counts that poison attempt and the drain can return Ok. The follow-up replaces the correlated derived-table aggregate with a flat MIN/MAX/COUNT aggregate and replaces INSERT SELECT UNION ALL with bound multirow VALUES. Cursor, epoch and boundary semantics and the six-statement count are retained. The exact Turso error was not supplied with the gate result; this diagnosis and fix await the coordinator's rerun.

`fresh_root_and_child_commit_all_three_messages` covers memory and syscall I/O without a warm-up drain. It checks root configure, three committed child domain rows, inbox completion, every commit-order/boundary/code-at receipt and an empty dead-letter table. Failures print the dead-letter errors before checking progress counts. No builds or tests ran in this write-only follow-up.
