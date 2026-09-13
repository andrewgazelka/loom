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
- Actor turns and timer scans are independent scheduler tasks. Wakeups during an idle check mark that actor for another turn, preventing a later idle result from hiding newly arrived messages.
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
