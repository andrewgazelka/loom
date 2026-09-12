# Future work

Every open item in one place, one file each, with the trigger that starts it and the check
that finishes it. Items are ordered by how much they change what a user can do. Nothing here
is scheduled; the plan tab of the status page and `docs/architecture.md` section 9 say what is
in flight. When an item lands, its file moves to `docs/` as design or is deleted, and this
index loses the row.

| Item | Why it matters | Trigger | Done when |
|---|---|---|---|
| [wal-frame-shipping.md](wal-frame-shipping.md) | fork at any seq without replay; smaller segments | Turso's public `Connection` exposes the frame API, or we take `turso_core` directly | `fork(id, seq)` reads no inbox rows; durability tests unchanged |
| [connect-hash-to-build.md](connect-hash-to-build.md) | `behavior_hash` becomes a content hash for real definitions; `code_changes` gets `wasm_hash`, `toolchain_hash` | encoder coverage landed (round 4) | a Loom definition's behavior hash is stable under rename/reformat; lineage shows item diffs |
| [incremental-per-lineage.md](incremental-per-lineage.md) | one-function candidate compiles in seconds | none; small | measured median one-function change on `rust-preview` before and after |
| [object-cache-default.md](object-cache-default.md) | build reuse across crates by content | measured win holds on the guest SDK (883 vs 1121 ms today) after admission widens | cache on by default; no regression on the micro fixture |
| [s3-live-test.md](s3-live-test.md) | proves conditional PUT fencing against a real object store | a MinIO in `nix run` | two-node takeover test passes against MinIO |
| [cells-and-distribution.md](cells-and-distribution.md) | actors move between machines; `monitor_node`; cross-node send | first workload larger than one machine | a message crosses nodes with the same delivery guarantees; lease takeover across machines |
| [tenant-isolation.md](tenant-isolation.md) | one VM per tenant as the outer blast radius | first untrusted tenant | a tenant cannot observe another's files or CPU |
| [limits-enforcement.md](limits-enforcement.md) | fuel and memory limits per actor are real traps | none; wasm behaviors exist now | a spinning behavior is poisoned by fuel, not a stuck thread |
| [repl-ui-actors.md](repl-ui-actors.md) | the REPL shows and drives actors through the MCP surface | old panel removed (round 4) | tree, inbox, lineage and validate verdicts visible in the UI, all through `actor_*` tools |
| [ci.md](ci.md) | Linux gate with io_uring, guest fixtures, the three ignored `loom-rt` tests | none | one CI run prints the same `test result:` lines as the local gate, on Linux |
| [validation-fitness.md](validation-fitness.md) | the LLM evolution loop needs a fitness signal beyond Matched/Differs | first self-evolving supervisor | a supervisor promotes a candidate only after its SQL assertions pass on the fork |
| [population-queries.md](population-queries.md) | "which actors ran hash H last week" across the node | more than a few hundred actors | a derived query store fed by the segment stream; actors never read it |
| [turso-cdc-and-views.md](turso-cdc-and-views.md) | logical change capture and incremental views instead of hand-maintained derived tables | Turso marks them stable | validator's logical diff reads `turso_cdc`; derived tables are views |
| [sfi-on-cranelift.md](sfi-on-cranelift.md) | an option, not a plan: sandbox without the wasm stage | measured per-candidate compile time or guest CPU overhead above a stated bound | never, unless the trigger fires |
| [deterministic-replay-option.md](deterministic-replay-option.md) | bit-for-bit replay for pure definitions | a debugging need | a pure definition replays identically across hosts |
