# Multi-node Loom: one cluster, one object store, no consensus

Status: design v1, 2026-09-12. Fills the `docs/future/cells-and-distribution.md` row.
Scope: several `loomd` processes on several hosts sharing ONE object store and ONE
cluster key. One cell. No Raft, no gossip, no wire protocol beyond HTTP JSON on the
listener `loomd` already has.

Read `docs/architecture.md` first, then `docs/actors-turso.md` Addendum C (durability
and leases) and the capability section. This document only adds what is missing
between "one node" and "many nodes"; every guarantee below is the single-node
guarantee restated across a host boundary.

## The goal, as a command

    cargo test -p loom-actor --test cluster

prints `test result: ok. 8 passed` when v1 is done. Baseline 2026-09-12: the test
target does not exist (0/8). The eight tests are listed at the end; each is a
property. The executable proof on real processes is

    scripts/cluster-e2e.sh

which starts two `loomd` on 127.0.0.1:8801 and :8802 with `--store local:<dir>` and
prints `N/5` (its five checks are in the last section). Baseline: script absent.

## What exists today (measured against the tree at 073df5f)

- One `loomd` = one `loom_actor::Node`: a directory of Turso files, `_node.db`, a
  registry of behaviors, one HTTP/MCP listener (`crates/loomd/src/main.rs:70`).
- Actor ids are already globally unique: root `a0<ULID>`, child
  `a0<blake3(parent||seq||idx)[..26]>` (`crates/loom-actor/src/ids.rs`). The `a0`
  prefix is the reserved cell byte; this design keeps it fixed.
- Durability to an object store is BUILT (`Config.store`, `remote_store.rs`,
  `durability*.rs`, `tests/durability.rs`, 5 tests): per-actor snapshots and logical
  segments under `actors/<id>/`, a `head` pointer written by conditional PUT, and a
  per-actor `lease` `{owner, epoch, expires_at}` acquired on first open, renewed at
  TTL/3, and taken over with a higher epoch after expiry. `docs/architecture.md` §6
  still says "not yet implemented" for this; it is stale, Addendum C is current.
- Today's rule when another live owner holds the lease: `remote_store.rs:147`
  `acquire` FAILS ("lease held by <owner> until <t>"). There is no notion of "someone
  else legitimately runs this actor, talk to them". That one branch is the whole
  gap between durability and distribution.
- Capabilities are BUILT (`capability.rs`, `cap_ops.rs`, `tests/caps.rs`, 6 tests):
  MAC = `blake3::keyed_hash(node_key, ...)` with `node_key` generated per node and
  kept in `_node.db.meta`. The spec says "a capability minted by another node has no
  authority here". For a cluster that sentence must become "minted by another
  CLUSTER".
- The pump (`pump.rs`) delivers committed outbox rows by target kind: actor id,
  `spawn`, `effect:<kind>`, `down:<watcher>`, `exit:<peer>`, `stop:<id>`,
  `shutdown:<id>`, call replies. Every kind ends in one keyed, idempotent write to
  ONE target file. A failed destination blocks only its own later rows (per-pair
  FIFO).

## The one new rule: the lease says who runs an actor, and everyone else routes

Definitions used below:

- **node**: one `loomd` process. Identity `node_id` = a ULID persisted in
  `_node.db.meta` (`node_id`) on first start, so restarts keep it. Configured with
  `--advertise <host:port>`, the address other nodes reach its listener on.
- **cluster**: the set of nodes sharing one `--store` and one `--cluster-key-file`
  (32 bytes). The object store is the ONLY shared state. There is no membership
  list to agree on: a node is in the cluster because it can read the store and
  holds the key.
- **owner** of an actor: the node named in `actors/<id>/lease.owner` while
  `expires_at` is in the future. `owner` becomes the node_id (today it is a
  per-`Node`-instance random string; same field, stable value).
- **placement** of an actor: `Local` (this node owns it), `Remote{node_id, addr}`
  (another node owns it, lease unexpired), `Unowned` (no lease or expired lease).

Rule: before any operation on an actor file, resolve placement.

| placement | action |
|---|---|
| `Local` | today's path, unchanged |
| `Unowned` | today's `acquire` + restore (takeover with epoch+1), then Local. This is "the ingress boots a cold actor on demand": no separate boot step exists |
| `Remote` | do NOT open the file. Forward the operation to the owner's ingress (below). Operator reads (`info`, `sql`, `lineage`, `dead_letters`) forward the same way |

Address resolution: `nodes/<node_id>` object `{addr, started_at, expires_at}`,
written by the node at start (conditional create or update by the same node_id),
renewed on the lease renewal tick, TTL = `Config.lease_ttl`. It exists ONLY to map a
lease owner to an address; liveness is the lease's own expiry, never this object.
A node caches placements (`HashMap<ActorId, Placement>` with the lease's
`expires_at`); a cache entry is dropped on expiry, on any ingress error, and on a
409 from the owner. Locally owned actors never consult the cache (the `owned` map in
`remote_store.rs` already answers).

Consequence for node death: nothing happens at the moment a node dies. Leases it
held expire within `lease_ttl` (default 10 s). The next send, spawn, restart, or
operator call that resolves one of its actors finds `Unowned`, takes over, restores
from the store, and runs it on the node that asked. `monitor_node` therefore has no
meaning in v1 and is dropped from the future list: a watched actor on a dead node is
not down, it is unowned, and the next message revives it elsewhere. What a monitor
sees is at most `lease_ttl` + restore time of extra latency.

## Ingress: the pump's destination step, executed on the owner

`POST /v1/ingress` on the existing axum router (`crates/loom-api/src/http.rs`).

Request: `{"ops":[{kind, target, key, sender, msg, ...}, ...]}`, one entry per outbox
row, in the sender's `(seq, idx)` order, all for targets this node is believed to
own. `kind` enumerates EXACTLY the pump's destination kinds (inbox message, spawn
child, restart/stop/shutdown, monitor/link bookkeeping, call reply, revoke), each
carrying the same fields the local `deliver_*` function takes today. The receiver
runs those same functions. There is no second delivery implementation: the pump's
local branch and the ingress handler call one `Node::apply_delivery(op)`.

Response per op: `{ok:true}` after the write is durable (next section); or
`{ok:false, owner, addr}` with HTTP 409 when this node does not own `target`
(lease moved or expired), and the sender re-resolves; or 401 on a bad cluster
credential, with nothing written.

Auth: `Authorization: Bearer <hex(blake3::keyed_hash(cluster_key, b"loom-ingress-v1"))>`.
This bearer is valid on `/v1/ingress` only; user tokens are refused there (403), and
the ingress bearer is refused on every other route (403). The check lives beside
`authorize_token` in `http.rs`, not in a second middleware.

Transport: plain HTTP over whatever network the nodes share (a tailnet in our
deploys). TLS is the network's job in v1, stated here so nobody assumes otherwise.
Replay of an ingress request is harmless (every op is keyed and idempotent), so the
bearer being static is acceptable; it never crosses a non-cluster route.

Delivery semantics, per pair (sender actor, target actor):

- at-least-once transport, exactly-once effect: `INSERT OR IGNORE inbox(key)` on the
  receiver, unchanged. The sender marks the outbox row delivered only after `ok:true`.
  A crash on either side between commit and mark re-sends; the key dedupes.
- FIFO: the sender's pump already blocks later rows for a destination that failed;
  the ingress applies a batch in order and stops at the first failure, reporting
  how many ops were applied, so the sender marks exactly those delivered.
- fencing: the sender's `check_lease(sender)` runs before every delivery today and
  stays; a stale owner (lost lease mid-handler) cannot push its outbox across the
  boundary. The receiver checks its own lease on `target` inside the same
  transaction that inserts the row.

## Output gate: nothing crosses a node before it is durable in the store

Single-node Loom delivers a committed outbox row immediately and ships the segment
later (`Local` durability, `ship_interval` 1 s). That is safe on one host: a crash
loses the sender's tail and the receiver's tail together. Across hosts it is not:
node A could deliver a send, die, be restored from the store at an older seq, re-run
the message and take a different path, while node B already acted on the first
version.

Invariant 16 (new, joins the list in `docs/architecture.md` §2): **a message leaves
its node only after the transaction that produced it is in the object store, and an
ingress acknowledges only after the inbox row it wrote is in the object store.**

- Sender side: the pump, before forwarding a row whose target is `Remote`, requires
  `shipped_seq(sender) >= row.seq`; if not, it calls `Node::ship(sender)` and then
  forwards. `Remote`-durability actors already satisfy this at commit.
- Receiver side: the ingress inserts the inbox row, commits, and then holds the HTTP
  response until the target's next ship covers that row. Acks coalesce: one ship
  tick durably publishes every inbound row that arrived during the interval, and
  all their responses release together. The sender's pump is already per-pair
  blocking, so a held response stalls only that pair. `Remote`-durability targets
  ship at commit and respond at once.
- Same-node delivery keeps today's behavior (no gate). This is the only place where
  "local" and "remote" targets differ in the pump, and it is one `if` on placement.

Cost to measure and record here: cross-node send latency p50/p99 with `--store
local:` and with MinIO on hydra, and end-to-end `lease_ttl` + restore time for the
takeover test. Numbers, with host, date and denominator, replace this sentence.

## Spawn, move, and where actors live

- `spawn` is local: a child's file is created on the parent's node and its lease
  acquired there (unchanged). Placement policy beyond that is not in v1.
- `move(id, node_id)` is the one operator verb (CLI, HTTP, MCP): the requester
  forwards `{kind: release}` to the owner's ingress; the owner waits for the
  actor's connection lock (any in-flight message finishes), ships, releases the
  lease, renames its file `<id>.stale.<epoch>.db` (the existing `lease_lost` path,
  invoked deliberately), and responds. The requester then sends `{kind: adopt}` to
  the target node's ingress, whose `open` acquires and restores. `move` to self is
  adopt without release. A node that is not the owner receives 409 for `release`.
- Supervision across nodes needs no new code path: restart verbs, `stop`,
  `shutdown`, links, monitors and `Ctx::inspect` are outbox rows with targets, so
  they ride the ingress like any send. A supervisor on node 1 restarting a child
  that migrated to node 2 sends `spawn{restart}` to node 2's ingress.

## Capabilities across nodes

`--cluster-key-file` (32 bytes, 0600, provisioned out of band) replaces the
per-node generated key. `Node::new` reads it, stores `blake3(key)` in
`_node.db.meta.cluster_key_hash`, and refuses to open a `_node.db` whose hash differs
("cluster key changed; this node's caps and ingress trust are void, start a new
actors directory"). No migration: an existing single-node directory with a
generated key is rejected the same way (regenerate). MACs, epochs, `revoked` rows
and `caps` tables travel inside actor files, so a cap minted on node 1 verifies on
node 2 after a move with no extra state. Attenuation, revocation and `bump_epoch`
are unchanged. Cross-cluster caps remain invalid, which is the isolation boundary
`docs/future/tenant-isolation.md` builds on (one cluster per tenant).

## Operator surface

| verb (CLI = HTTP = MCP, same names) | does |
|---|---|
| `nodes()` | list `nodes/` objects: node_id, addr, started_at, live (lease-TTL rule) |
| `whereis(id)` | existing name lookup, plus for an actor id: placement (`local` / `remote{node_id, addr}` / `unowned`) |
| `move(id, node_id)` | as above; returns the new owner and epoch |
| `actors()` | today: this node's files. v1: `owner` column added; a `--cluster` flag lists `actors/` in the store with each lease owner |
| `info`, `sql`, `lineage`, `dead_letters`, `tree` | forward to the owner when placement is `Remote`; `tree` follows children across nodes because `children` rows live in files |

`loomd` flags added: `--node-id` (default: persisted ULID), `--advertise
<host:port>` (required when `--store` is set), `--cluster-key-file <path>` (required
when `--store` is set), `--store local:<dir> | s3://<bucket>?endpoint=&region=`
(maps onto the existing `StoreConfig`). Missing any of the three with `--store`
present is a startup error naming the flag.

## Deliberately not in v1

- Cells: one store per cluster; the `a0` prefix stays fixed. A second store is a
  second cluster with no route between them.
- Cluster-wide names: `register`/`whereis` by name stay per node (`_node.db`). A
  later round puts `names/<name>` in the store with conditional create.
- Placement policy: spawn is local, `move` is manual. No load balancing.
- `monitor_node`: replaced by lease expiry semantics above.
- Consensus, gossip, membership: the store's conditional PUT is the only arbiter.
- TLS on the ingress: the network's job.
- WAL-frame shipping: independent (`docs/future/wal-frame-shipping.md`); the output
  gate uses whatever `ship` does.

## Code map (the files a lane touches)

| file | change |
|---|---|
| `crates/loom-actor/src/cluster.rs` (new) | `NodeIdentity{node_id, addr}`, `nodes/<id>` write/renew, `Placement`, `resolve(id)`, placement cache |
| `crates/loom-actor/src/ingress.rs` (new) | `DeliveryOp` enum mirroring pump kinds; `Node::apply_delivery(op)` used by BOTH the local pump branch and the ingress handler; `Node::apply_ingress(ops) -> Vec<Ack>` with the held-ack output gate; the HTTP client `forward(addr, ops)` |
| `crates/loom-actor/src/pump.rs` | resolve placement per destination; `Remote` => output gate + `forward`; `Unowned` => acquire then local |
| `crates/loom-actor/src/remote_store.rs` | `owner` = node_id; `nodes/` objects; `release(id)` already exists, `move` reuses it |
| `crates/loom-actor/src/node.rs`, `types.rs` | `Config.cluster: Option<ClusterConfig{node_id, addr, key}>`; refuse `store` without `cluster` |
| `crates/loom-actor/src/capability.rs`, `initialize.rs` | key from cluster config; `cluster_key_hash` check at open |
| `crates/loom-api/src/http.rs`, `auth.rs` | `POST /v1/ingress`; ingress bearer, route-exclusive both ways |
| `crates/loom-mcp/src/actors.rs`, `crates/loom-cli` | `nodes`, `whereis` placement, `move`, `actors --cluster`; forwarding for reads |
| `crates/loomd/src/main.rs` | the four flags |
| `crates/loom-actor/tests/cluster.rs` (new) | the eight tests, two `Node`s in one process sharing one `StoreConfig::Local` dir and two in-process axum listeners on 127.0.0.1:0 |
| `scripts/cluster-e2e.sh` (new) | two real `loomd`, five checks, prints `N/5` |
| `docs/architecture.md` | §2 invariant 16, §6 durability rows marked built, §8 distribution row rewritten to point here |
| `docs/future/cells-and-distribution.md`, `docs/future/README.md` | row removed when the goal command prints 8 passed |

## Tests (the goal number)

Each test builds two nodes `n1`, `n2` over one local store dir with a test `Clock`,
spawns a `counter-v1` actor `B` on `n2` and a `forwarder-v1` actor `A` on `n1`
unless stated.

1. `two_nodes_pair_fifo`: `A` sends 100 messages to `B` (sends go through n2's
   ingress); `B.inbox` order equals `A.outbox` `(seq, idx)` order; count = 100 with
   `A`'s outbox rows all `delivered=1`.
2. `cold_actor_boots_on_send`: `n2.close()`; `A` sends to `B`; `B` now runs on `n1`
   (`whereis(B) = local` on n1, lease epoch incremented) and has the row.
3. `owner_death_moves_actor_within_ttl`: drop `n2` without `close` while `B` has 3
   committed and shipped messages; advance the clock past `lease_ttl`; `A` sends;
   `B` on `n1` has cursor 4 and exactly 4 inbox rows (no duplicate of the shipped
   three).
4. `output_gate_holds_unshipped_send`: `A` is `Local` durability with a paused ship
   worker; `A` sends to `B`; `B.inbox` on `n2` is empty; resume ship; the row
   arrives. The negative control is the same test with `Remote` durability, where
   the row arrives without resuming.
5. `ingress_ack_waits_for_receiver_ship`: `n2`'s ship worker paused; `A`'s forward
   call is pending (not acked) and `A.outbox.delivered=0`; resume ship; acked and
   marked. Crash `n2` before resuming instead: after takeover of `B` by `n1`, the
   inbox row is absent, `A` re-sends, one row.
6. `ingress_rejects_wrong_cluster_key`: a node with a different key posts to `n2`'s
   ingress: 401, `B.inbox` unchanged; a user token on `/v1/ingress`: 403; the
   ingress bearer on `/v1/command`: 403.
7. `cap_survives_move`: mint a SEND cap for `B` on `n2`; `move(B, n1)`; the cap
   authorizes a send executed on `n1`; a cap MACed with another key traps with the
   existing "invalid authority" dead letter.
8. `stale_owner_cannot_forward`: `B`'s handler on `n2` blocks; clock passes
   `lease_ttl`; `n1` takes over `B`; `n2`'s handler finishes and its pump tries to
   forward: fenced (`check_lease` fails), nothing inserted anywhere; `n2`'s file is
   `<B>.stale.<epoch>.db`.

`scripts/cluster-e2e.sh` (real processes, `N/5`): (1) both nodes list each other in
`nodes()`; (2) `loom send` to an actor spawned on the other node returns its cursor;
(3) `whereis` reports `remote{node_id, addr}` from the non-owner; (4) `move` then
`whereis` reports `local`, and the old owner's dir holds the `.stale.` file; (5)
`kill -9` the owner, wait `lease_ttl`, `send` from the survivor succeeds and `info`
shows the pre-kill cursor + 1.
