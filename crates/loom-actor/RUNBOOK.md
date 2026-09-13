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

`Config.io` selects `Auto`, `Syscall`, `IoUring`, or `Memory` through Turso's named VFS. Auto selects io_uring on Linux (the target dependency unifies Turso's feature), syscall elsewhere. An explicit io_uring request fails with the platform name on other systems. Memory keeps actor and directory-index databases in live connections and creates no database files; it has no disk snapshots or persistence across node lifetimes. In-memory logical snapshots now support fork, validation, and reset; this addition awaits the consolidated gate below. The `memory_io_is_wired` test exercises three messages and checks that the directory remains empty.

Kill and shutdown deadlines cancel only the native handler future, then await the message transaction rollback. They do not abort BEGIN, COMMIT, snapshot publication, or lifecycle transactions. Turso 0.7.2 marks `Transaction::in_progress` false only after awaited COMMIT returns; dropping the task between the engine commit and that assignment queues an invalid rollback on the next connection access. The io_uring parity run exposed this completion window; the corrected cooperative cancellation path passed the full 22-test actor suite on Linux with Auto selecting io_uring, as well as the local macOS suite.

With an explicit syscall/io_uring VFS, Turso treats `:memory:` as a literal filename. Scratch connections used while replacing a cached connection therefore select `Io::Memory` explicitly, independently of the node’s persistent I/O selection. The reset regression passes without creating `:memory:` or `:memory:-wal` files.

On Linux hosts with an 8 MiB locked-memory limit, concurrent io_uring-backed tests can fail with `io_uring_setup: out of memory` despite ample ordinary RAM. The 22 actor tests and 4 MCP tests pass with `RUST_TEST_THREADS=4 cargo test -p loom-actor -p loom-mcp` on such a host. Size test concurrency against ring-allocation headroom; keep the selected VFS unchanged.

## View actors and CDC

Status: authored, not compiled or executed. The user’s 2026-08-25 test-at-end directive reserves all gates for the lead. Source inspection and `git diff --no-ext-diff --check` are the only verification performed here. Earlier passing results in this runbook do not cover this change.

Run from the worktree root, resolving the new UI dependency and recording the resulting `ui/bun.lock` first:

```sh
(cd ui && bun install)
cargo test -p loom-actor --test view
bun test ui/tests/bind
cargo test -p loom-actor -p loom-behavior -p loom-api -p loom-mcp -p loom-cli -p loom-proto
cargo clippy -p loom-actor -p loom-behavior -p loom-api -p loom-mcp -p loom-cli -p loom-proto --all-targets -- -D warnings
(cd ui && bun run check)
cargo build -p loomd
LOOMD=target/debug/loomd scripts/ui-e2e.sh
```

Required results are 9/9 named actor tests, 5/5 named binding tests, and `4/4` from the native daemon e2e script, plus the broader regression and type/lint gates above. The script copies the supplied daemon executable, owns its temporary directory and process, and checks its listening log before probing HTTP. The browser-node document’s wasm `cargo check` command is a phase-2 target, not a claim that this native implementation builds for wasm.

Dependencies: `turso_core = "0.7.2"` becomes a general loom-actor dependency for the engine’s record decoder; the existing Linux io_uring feature dependency remains. This version already exists in Cargo.lock. UI tests add exactly pinned `happy-dom` 20.14.3 because the existing tests have no DOM shim. Its dependency graph and lockfile update remain for `bun install`; no dependency installation ran here. No other dependency was added.

Deviations, elaborations, and limits:

- Ephemeral history uses logical in-memory snapshots, including physical rowids and schema objects, so fork/validation/reset need no disk file. Public forks are registered ephemeral actors; validation scratch actors are not registered. Parent child rows persist, while startup removes stale ephemeral index/subscription entries.
- Serving connections enable `PRAGMA capture_data_changes_conn = 'full'` with no table argument, as confirmed in Turso 0.7.2. Exact image restoration temporarily disables capture to avoid manufacturing CDC. Older CDC versions fail closed rather than being migrated implicitly.
- CDC does not encode DDL. Schema-changing turns publish a replacement snapshot and persist a repair marker until publication completes; selective receive waits for a contiguous history cut. Template-only promotion retains normal row CDC. Validation checks the snapshot plus domain CDC independently of behavior replay, with trigger execution suspended inside the replay transaction.
- Frames decode full before/after records into column objects, encode SQL blobs as byte arrays, and retain sparse `updates` as engine record bytes. Rowid tables are required by the existing history model. Virtual generated columns whose omitted CDC fields cause a schema-width mismatch fail closed; generalized generated-column replay is not implemented.
- Actor commits retain their inbox sequence and key. Control transactions without an inbox origin use the negative CDC transaction ID as `seq`, avoiding delivery-key collisions between promotions at the same inbox cursor. Delta frames carry `cause`: the immediately handled delta's key, or null. Incoming causes are never inherited. This implements the updated source -> view -> browser correlation rule; negative control sequences remain an explicit wire extension.
- View source rows are cached in runtime metadata so promotion can rerender all rows without inverting rendered trees. The single domain table remains `tree`. The behavior bridge adds `LoomTemplate` around the existing `Runtime::call_with_effects` entry point; it requires a single Value argument/result and rejects unknown or nonempty effect rows at admission.
- The existing definition-inspection `view(target)` verb remains as an exclusive overload of `view(actor, table, template, order_by)`. CLI actor-view arguments are named flags. HTTP, MCP, and CLI share the registry and execution permission checks.
- Browser inspection capabilities are opaque serialized strings; JSON numeric capabilities would lose u64 precision. Host subscribers still use pump subscription operations. Normal socket closure queues unsubscribe; restart pruning handles process loss and ephemeral subscribers.
- Confirmed HTTP send failure becomes a local dead-letter verdict for the binding. Pending execution and lost acknowledgements retain pending state. The shell marks the existing row pending rather than inventing application-specific optimistic edits.
- The patcher refuses changes to a living root/keyed-child tag and rejects active content and executable attributes. Children match by key, unkeyed elements by tag in document order, and text by position. Ordering runs after removals, moving neighbours with `insertBefore` around the stationary focused node or its ancestor, including row ordering. Selection/scroll are preserved without refocusing. In-flight transitions on moved neighbours remain unverified. The focus test removes a leading child, changes the unkeyed input's index and its row's index, and asserts unchanged activeElement, zero blur events, and no removal of either focused subtree. The pending test explicitly matches `cause` with a different frame key.
- The redelivery test models the committed-receiver/unacknowledged-outbox boundary by resetting the delivered flag; it does not kill a process. Actor tests use native template fixtures; compiled guest templates are exercised by the authored e2e script. Roadmap documents remain unverified until the lead records the gates.

Uncompiled assumptions requiring particular attention (paths relative to the repository root):

- `crates/loom-actor/src/cdc.rs:37`: Turso’s exposed ValueIterator/ValueRef APIs and numeric conversion match the inspected artifact; exact row replay also assumes caller-owned transactions restore suspended triggers on failure.
- `crates/loom-actor/src/history/memory.rs:64`: restoring explicit rowid alongside its INTEGER PRIMARY KEY alias is accepted; `:79` assumes schema_version changes identify transactional DDL.
- `crates/loom-actor/src/actor/snapshot.rs:32`: CDC compaction, subscriber cursor advancement, and metadata cleanup commit together with the engine’s own CDC bookkeeping.
- `crates/loom-behavior/src/template.rs:28`: the compiler reports pure JSON templates as Value-to-Value definitions with empty effects; `:55` assumes the existing runtime bridge accepts the one-element JSON argument array.
- `crates/loom-api/src/http.rs:308`: the websocket select loop and cleanup future meet Axum’s Send requirements, and ordinary close reaches pump unsubscribe. Abrupt task cancellation relies on restart pruning.
- `crates/loom-actor/tests/view_support.rs:95`: a second Turso connection observes the live WAL. `crates/loom-actor/tests/view.rs:77` assumes ephemeral spawn preserves baseline directory entries; `:198` assumes fresh counter inserts have rowids 1, 2, and 3.
- `ui/tests/bind/fixture.ts:37`: Happy DOM implements the browser methods used through the explicit DOM type boundary; `ui/src/lib/bind/patch.ts:173` needs real-browser confirmation that neighbour ordering preserves the stationary editor's focus/caret/scroll.
- `ui/src/routes/view/+page.svelte:92`: the typed binding callback and asynchronous teardown satisfy Svelte checking and lifecycle behavior. `scripts/ui-e2e.sh` additionally depends on the real daemon’s compiler prerequisites and pure template effect inference.

Every gate above remains unrun; these source-level assumptions are not successful compilation or production-integration evidence.

## Cluster (multi-node)

This lane authored code and tests only. No builds, tests, formatters, Nix commands or scripts were run. The lead must run the consolidated gate from this worktree; neither 8/8 nor 5/5 is claimed.

```sh
cargo fmt --all -- --check
cargo test -p loom-actor --test cluster
cargo test -p loom-actor -p loom-api -p loom-mcp -p loom-cli -p loom-proto
cargo clippy -p loom-actor -p loom-api -p loom-mcp -p loom-cli -p loom-proto -p loomd --all-targets -- -D warnings
cargo build -p loomd -p loom-cli
scripts/cluster-e2e.sh
```

The cluster target contains the eight exact spec names. The process script requires `jq`, the prebuilt binaries and the existing Rust guest toolchain. It admits `examples/unison/counter.rs` to both definition stores, prints `N/5`, and returns zero only for 5/5. Failed runs retain their isolated directory and logs; the gate owner removes them. Latency p50/p99 on local storage and MinIO, and measured takeover time, remain for the lead's execution gate.

Deviations and implementation choices relative to `docs/multi-node.md`:

- Supporting files split transport, publication waiting, movement, authority, relationships and test fixtures out of the code-map files to keep each changed Rust file below 400 lines. The shared destination implementation remains `Node::apply_delivery` and the existing delivery helpers.
- The requested `src/initialize.rs` is actually `src/actor/initialize.rs`, which initializes actor files. `_node.db` cluster-key validation instead runs in `capability::node_key`, called by `Node::new`; no actor-file key migration was added.
- Empty `ClusterConfig.node_id` selects the persisted/generated ULID. `--node-id` is optional as specified by its documented default, despite the conflicting sentence requiring all three flags. Explicit IDs use object-key-safe characters and must identify one node directory uniquely. Reusing a live node ID is not independently fenced by the spec's node-record format.
- `_node.db.meta.root_id` retains the local supervisor identity across adoption of another node's actors. `HostSpawn` routes host spawning if that supervisor has moved. Stale local connections and migrated files are archived before fresh ownership is admitted.
- Durable publication revisions are distinct from inbox/outbox sequence numbers in the existing implementation. The sender therefore flushes all pending changes before remote delivery rather than relying on `head.seq >= outbox.seq`. Receiver acknowledgements wait for the connection's `total_changes()` watermark to be covered by publication. Concurrent arrivals share the next shipment; waiters observe coverage every 5 ms.
- `pause_shipping`/`resume_shipping` gate explicit and background Local shipment for the required negative controls. Remote transaction publication bypasses this gate. This reconciles the spec's paused-worker test with its explicit sender `Node::ship` requirement.
- Destination jobs run concurrently within a sender pump, retaining ordered delivery and successful marking within each pair. The receiver-ack test includes a second destination that must progress while the first acknowledgement is held.
- Existing links, monitors, calls and shutdowns modify multiple actor files. `RelationshipWrite` splits those writes by owner through the same delivery dispatcher. Calls, links and monitor registration orchestrate on their sender. Monitor-removal receipts prevent delayed registration retries from resurrecting removed monitors.
- Ingress HTTP responses contain `{acks, applied}`; `applied` is the acknowledged successful prefix. `Ack.conflict` distinguishes HTTP 409 even when an expired lease has no replacement owner/address. The transport retains successful prefixes on a later server error, invalidates failed routes, disables redirects and bounds requests by `lease_ttl`.
- Operator `Command` entries dispatch through the existing API `ActorService`, since `loom-actor` cannot depend on `loom-api`. Mutating commands ship before acknowledgement. `State`, `Children`, `Authority`, `Mint` and `Inspect` provide typed internal reads needed by supervision and capabilities. The same HTTP ingress carries them.
- `actors --cluster` lists identity, owner and placement without acquiring unowned files merely to list them. Name-based `whereis` remains local. API/MCP/CLI commands use the shared verb registry.
- A cluster-only daemon worker schedules newly received messages; creating a `Node` alone still permits deterministic explicit stepping. Shutdown signals the worker, drains admitted turns, then closes the node. It does not abort a transaction to meet a shutdown deadline.
- The actor test's two-listener fixture shares the production bearer calculation but implements route classification locally to avoid an actor-to-API dependency cycle. `loom-api/src/tests/cluster_auth.rs` separately exercises the production middleware, rejected-write controls and a valid-ingress control. The wrong-cluster bearer is derived directly from another key. The stale-owner test uses a blocking forwarder so it actually attempts a send after expiry.
- The architecture and capability addendum now describe clustered keys and stable lease owners. Future distribution rows remain because the required executed 8/8 result is not available.

New direct dependencies, all already present in `Cargo.lock`: `reqwest 0.12` with JSON and without default features in `loom-actor`; `axum 0.8` and Tokio's `net` feature for actor tests; `url 2` in `loomd`. No new package version or heavy dependency family was introduced. Workspace package dependency lists in the lockfile were updated manually because Cargo execution was prohibited.

Specific uncompiled assumptions for the lead:

- `src/ingress.rs:165`, `src/pump.rs:115`, `src/relation_delivery.rs:19`: boxed recursive delivery futures satisfy `Send` through the scheduler and Axum handlers.
- `src/cluster.rs:167` and `src/cluster.rs:185`: the existing object-store listing stream supports the `poll_fn`/`poll_next` calls used here.
- `src/ingress_gate.rs:21`: publication's connection-local change watermark covers committed ingress writes; reset/reopen and cancellation interleavings need particular attention in the gate.
- `src/authority_ingress.rs:23` and `../loom-api/src/actors/cluster.rs:21`: typed authority envelopes, operator result envelopes and the ingress acknowledgement contract agree across crate boundaries.
- `src/scheduler.rs:32` and `../loomd/src/cluster_worker.rs:18`: Notify/watch APIs and cooperative shutdown compile with the workspace Tokio features and retain `Send` futures.
- `../loomd/src/main.rs:148` and `../loom-cli/src/operation.rs:43`: Tokio file `take/read_to_end`, URL parsing, and Clap boolean extraction match their locked crate APIs.
- `tests/cluster_support/mod.rs:91`: the in-process Axum handler is `Send`; the eight tests' shipping, process-drop and stale-file interleavings are authored but unexecuted. Formatting may move these line references; each entry identifies the relevant function or operation as well as its current location.

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
- Mailbox completion updates the inbox row, reads the contiguous cursor and historical-boundary predicate together, then writes epoch, commit order, cursor, optional boundary and code revision in one metadata statement. Overflow is checked in Rust before any completion write. The committed result supplies send outcomes and snapshot admission; each qualifying cursor is snapshotted before the next message, including inside a batch. The initial snapshot retry remains.
- Pumps read metadata and pending outbox rows once per batch. The metadata read includes persisted publication-receipt existence for recovery. Only a persisted receipt or a restart acknowledgment in this pump enables the publication-row query. Host SQL mutations wake a fresh pump. Receipt writes/deletes and child readiness still commit through control transactions.

For a local memory no-op, with no timer, publication, dirty index or shutdown work, the previous path issued **28 SQL statements per message**: admission status/ready/generation/cursor (4), mailbox epoch/selection (2), code (1), BEGIN (1), attempt generation (1), completion inbox update/epoch read/epoch write/order write/cursor aggregate/cursor write/boundary probe/boundary write (8), code-at write (1), COMMIT (1), outcome cursor/state and final status (3), pump status/replay-source/generation/outbox/publications (5), scheduler probe (1).

The new path issues **6 per message**: mailbox selection, BEGIN, inbox completion update, cursor/boundary aggregate, combined metadata write, COMMIT. Each batch adds 6 admission reads and 2 pump reads; discovering an empty inbox adds one selection. A preloaded 1,000-message no-op therefore has 6,129 statements across 16 batches, excluding setup and external injection. Handler SQL/effects, remote shipping, outbox delivery acknowledgments, timer checks, lifecycle work and disk snapshots add their own statements. These are source counts, not timings. The under-300-us targets and the existing suite remain for the coordinator to measure.

Mechanism coverage in `tests/scheduler.rs` adds 100 exact completions in two to three scheduler tasks, and a mid-batch deferral which ends its task, commits the releasing message, then retries the deferred message in selective-receive order. No builds or tests were run in the write-only lane.

Adjacent observations, left unchanged: mailbox selection still sorts unfinished rows by selective-receive priority; completion still aggregates the inbox on every message. Each outbox delivery still reads the source generation and uses a receiver transaction plus a source acknowledgment transaction. Capability authorization and outbox-position reads remain per send. These costs can become the next floor, especially for broadcast, after scheduler overhead falls.

### Completion regression follow-up

The coordinator's gate for integration commit `3fa9c5c` failed five targets; the integration target had 1 pass and 26 failures. Fresh-message domain writes were absent and actors were parked. Source tracing finds live admission reads, committed publication wakes and a root wake during Node initialization. Node initialization enqueues configure; it does not itself drain that message before host spawn.

The new completion SQL is the remaining source-level suspect: a completion error is a runtime Trap, so retries roll back domain writes and exhausted retries poison/park the actor. Scheduler progress counts that poison attempt and the drain can return Ok. The follow-up replaces the correlated derived-table aggregate with a flat MIN/MAX/COUNT aggregate and replaces INSERT SELECT UNION ALL with bound multirow VALUES. Cursor, epoch and boundary semantics and the six-statement count are retained. The exact Turso error was not supplied with the gate result; this diagnosis and fix await the coordinator's rerun.

`fresh_root_and_child_commit_all_three_messages` covers memory and syscall I/O without a warm-up drain. It checks root configure, three committed child domain rows, inbox completion, every commit-order/boundary/code-at receipt and an empty dead-letter table. Failures print the dead-letter errors before checking progress counts. No builds or tests ran in this write-only follow-up.
