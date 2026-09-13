# UI as a view actor: the DOM is a materialized view of actor tables

Status: design v1, 2026-09-12. Fills `docs/future/repl-ui-actors.md` and
`docs/future/turso-cdc-and-views.md`; names `docs/future/browser-node.md` as its
phase 2. Read `docs/architecture.md` first, then `docs/actors-turso.md` (the
capability section and Addendum C), then `docs/multi-node.md` (the ingress; a
browser is a client of the same ops).

## The goal, as a command

    cargo test -p loom-actor --test view

prints `test result: ok. 9 passed` when v1 is done (baseline 2026-09-12: target
absent, 0/9), and

    bun test ui/tests/bind

prints `5 pass` (baseline: directory absent, 0/5). The executable proof on a real
daemon is `scripts/ui-e2e.sh`, printing `N/4`.

## One primitive: the row delta

Turso (0.7.2, the engine under every actor file) has change data capture built in:
`PRAGMA capture_data_changes_conn = 'full'` on a connection makes every INSERT,
UPDATE and DELETE (DML; schema statements are not captured) append a row to a
`turso_cdc` table in the same transaction (`turso_core-0.7.2/translate/pragma.rs:591`,
`vdbe/execute.rs:12944`):

    turso_cdc(change_id INTEGER PRIMARY KEY AUTOINCREMENT, change_time, change_txn_id,
              change_type, table_name, id, before BLOB, after BLOB, updates BLOB)

The rows of one `change_txn_id` are the exact delta of one message. Loom already
records, replays, ships and validates per message; this design makes the same delta
drive the screen. Nothing in it computes a diff: the diff is written by the engine at
the moment the data changes.

Every actor connection runs with CDC `full`. `turso_cdc` is an append-only runtime
table: guests cannot write it (`guest_sql.rs` refuses runtime tables), it ships in
segments like every other table, and it is compacted at snapshot time (rows with
`change_id` below the snapshot's `cdc_floor` are deleted; the floor is a `meta` key;
its leaver is the next snapshot).

Invariant 17 (joins `docs/architecture.md` §2): **the `turso_cdc` log of an actor,
replayed onto its last snapshot, reproduces its domain tables byte for byte.** Test 1
below enforces it against the existing builtin behaviors; the validator gains a
second verdict source from it (`docs/future/turso-cdc-and-views.md` done-when).

## Subscriptions: an actor-level primitive, one mechanism for actors and browsers

    cx.subscribe(&cap, table)      -> subscription_id     // needs INSPECT on the target
    cx.unsubscribe(subscription_id)

A subscription is a row in the TARGET's runtime table
`subscribers(id, subscriber, table, after_change_id)`. After every commit of the
target, the pump reads `turso_cdc` rows with `change_id > after_change_id` for
subscribed tables and delivers them as ONE inbox message per subscriber per commit:

    {"type":"delta", "source":<actor>, "seq":<source seq>, "key":<inbox key of the
     message that produced it, or "control">, "rows":[{change_id, change_type, table,
     id, before, after, updates}, ...]}

keyed `delta:<source>:<seq>:<subscriber>`, so redelivery is a no-op like every
other send. A frame also carries `cause`: the `key` of the delta message the
producing actor was handling, or null. A view's `tree` deltas therefore carry the
source's message key as `cause`, and a browser reconciles its optimistic row on
`cause` (one hop away) or `key` (direct subscriber). Causes do not chain further:
two hops is the designed depth (source -> view -> browser). The first message after subscribing is `{"type":"snapshot", ...rows}`
for the table (built from `SELECT *`, tagged with the `change_id` it is current
as of). A subscriber that falls behind the CDC floor (its `after_change_id` was
compacted) gets `{"type":"resnapshot"}` and a fresh snapshot: fail closed, never a
gap. Subscriptions ride the outbox and the pump, so they cross nodes through the
ingress in `docs/multi-node.md` with no extra code.

The browser is a **host-side subscriber**: `/v1/stream` gains
`{"subscribe": {"actor", "table", "cap"}}` which registers a subscriber row whose
target is `ws:<connection id>`; the pump delivers those frames to the socket
instead of an inbox. Same row, same frames, same floor rule. A closed socket
removes its rows (the leaver).

## Durability::Ephemeral

`ChildSpec.durability` gains `Ephemeral` beside `Local` and `Remote`. An ephemeral
actor's file uses Turso's memory VFS (the `Io::Memory` path that exists at node level
today, `types.rs:248`, applied per file), takes no lease, ships nothing, and has no
`actors/<id>/` prefix in the store. Everything else is identical: inbox, effects,
outbox, `turso_cdc`, code_changes, fork, validate. Node restart forgets it; its
parent's `children` row is the record that it existed, and the parent's supervisor
policy decides whether to respawn (`temporary` by default for ephemeral children).
`Ephemeral` is also what forks and validation scratch actors use from now on.

## The view actor

A view is an ordinary actor running the builtin native behavior `view-v1`, always
`Ephemeral`, spawned by the operator (`view(actor, table, template, order_by)`) or
by a behavior (`cx.spawn` with `view-v1` and that init message):

- init: `{"source": cap, "table": T, "template": <definition hash>, "order_by": [...]}`
  subscribes to `T` on `source` and records `template` as its first `code_changes`
  row. **The template is the view's behavior hash**: promote = hot reload.
- on `snapshot` / `delta`: for each affected row, call the template definition
  through `loom-behavior` (definitions already run inside actor transactions) as
  `render(row: Value) -> Tree` and upsert into the view's one domain table

      tree(key TEXT PRIMARY KEY, sort BLOB, tree BLOB)     -- tree = JSON node

  Deletes delete. Because `tree` is a domain table, the view's own `turso_cdc`
  carries `{key, sort, tree}` deltas, and the browser subscribes to THAT. The
  browser never sees the source table; it sees keyed trees.
- on promote (new template hash): re-render every row of `tree` in one transaction.
  The browser receives one delta with every key updated and patches each node in
  place. This is the whole hot-reload path; there is no other.
- `validate(view, candidate_template, k)` is unchanged actor validation: replay the
  last k deltas under the candidate and report `Matched`/`Differs{tree}`. A template
  can be checked against real history before it is promoted.

Template definitions are Rust (invariant 15: one guest language): a crate-root
`pub fn render(row: loom::Value) -> loom::Value` with an inferred effect row of `[]`;
`view-v1` refuses a template whose row is non-empty at spawn and at promote, naming
the effect. The returned value is a tree:

    {"tag":"li", "key":"row-42", "attrs":{"class":"row"}, "children":[ "text", {...} ]}

`key` is required on every child that is a list item and is the DOM identity below.

## The browser side: keyed DOM binding, no framework, no bundler

`ui/src/lib/bind/` is plain TypeScript with no Svelte or Vite dependency, importable
as ES modules and unit-tested under `bun test` with the DOM shim the existing
`ui/tests` use (the lane checks which one and says so in the runbook):

    bind(container: Element, stream: DeltaStream, opts: { onEvent })

- `Map<key, {node: Element, tree: Tree}>`. `snapshot` builds every node once, in
  `sort` order. `delta` rows: insert => build + `insertBefore` at the sort
  position; update => patch the existing node from `old tree -> new tree` (a per-row
  patch of two small JSON trees: attributes set/removed, text replaced, keyed
  children matched by `key`, unkeyed element children by tag in document order,
  text by position; only unmatched nodes are created or removed; ordering is a
  second pass after removals, and the node that holds focus is never moved,
  because a DOM move blurs: its neighbours move around it); delete => remove. A node is never replaced while its key lives, so
  focus, caret, scroll and in-flight transitions survive every update and every
  promote.
- events: `attrs` whose name starts with `on` are wired to `opts.onEvent(key, name,
  payload)`; the shell turns them into `send(cap, msg)` with a client-generated
  message key.
- optimistic rows: the shell may call `bind.pending(key, tree, messageKey)`; the node
  renders with `data-pending`. The first `delta` whose frame `cause` or `key`
  equals `messageKey` clears it; a `dead_letter` frame for that key reverts to the last
  authoritative tree and surfaces the trap text. No merge logic exists: the source
  actor is the single writer and the delta is the verdict.

`/v1/cas/<hash>` is served with `Cache-Control: public, max-age=31536000, immutable`
(it is content-addressed; today it sends no cache header, 0 hits in `loom-api`).

## What this replaces and what it does not

- Replaces: tree diffing for data changes (the engine writes the delta), a client
  state store (the mirror is the `tree` table), a second event API for the UI
  (subscriptions are actor sends), and bundler HMR for the parts of the UI that
  are views (promote is the reload, with a verdict).
- Does not replace: the SvelteKit shell (`ui/`) for chrome, palette, editors. It keeps
  Vite for its own dev loop; the binding core does not import from it. When the shell
  stops changing, Vite can go and nothing here notices.
- CRDTs: not needed. One writer per actor file (invariant 12) sequences concurrent
  edits; optimistic rows reconcile by message key. A collaborative-text column can
  hold a CRDT value later; the runtime never learns that it is one.

## Phase 2 (not in this lane): the browser as a node

Move the view actor into the tab: `loom-actor` on `wasm32-unknown-unknown` with the
memory VFS, templates instantiated by the browser's own WebAssembly engine (the
guest ABI uses shared memory with atomics, `docs/shared-core-abi.md`, which in a
browser requires cross-origin isolation headers on the page), and the tab as a node
that owns only ephemeral actors and speaks the ingress ops over the WebSocket with
capabilities. Its first command is `cargo check -p loom-actor --target
wasm32-unknown-unknown`; its error list is that lane's plan. Nothing in phase 1
changes when it lands: the browser already consumes `tree` deltas; only where they
are computed moves.

## Deliberately not in v1

- Placement of views (they run where spawned); partial subscriptions (`WHERE`): a
  view with a filtering template is the same thing; server-side pagination; binary
  columns in trees (base64 them in the template); CSS handling beyond `class` and
  `style` attributes; transitions.

## Code map

| file | change |
|---|---|
| `crates/loom-actor/src/types.rs` | `Durability::Ephemeral`; `Config` unchanged |
| `crates/loom-actor/src/initialize.rs`, `actor.rs`, `durability*.rs` | per-file memory VFS for ephemeral; skip lease/ship; forks and validation scratch become ephemeral |
| `crates/loom-actor/src/schema.rs`, `actor.rs` | CDC pragma on every connection; `subscribers` table; `cdc_floor` meta; compaction at snapshot |
| `crates/loom-actor/src/pump.rs` (+ `subscribe.rs` new) | after-commit delta fan-out to `subscribers`; `sub:`/`ws:` targets; snapshot and resnapshot frames |
| `crates/loom-actor/src/lib.rs` (`Ctx`) | `subscribe`, `unsubscribe` (INSPECT right) |
| `crates/loom-actor/src/builtin.rs` (+ `view.rs` new) | `view-v1`: init, delta handling, promote re-render, effect-row refusal |
| `crates/loom-behavior` | call a definition by hash with a `Value` argument from inside a native behavior (check what exists; the bridge already runs definitions in transactions) |
| `crates/loom-api/src/http.rs` | `/v1/stream` subscribe frames; `ws:` subscriber delivery; immutable cache header on `/v1/cas/{hash}` |
| `crates/loom-mcp/src/actors.rs`, `crates/loom-cli`, `crates/loom-proto/src/verbs.rs` | `view(actor, table, template, order_by)`, `subscriptions(id)`; `spawn` accepts `durability: ephemeral` |
| `ui/src/lib/bind/{bind.ts,patch.ts,stream.ts}` | the binding core; `ui/tests/bind/*.test.ts` |
| `ui/src/routes/view/+page.svelte` | a shell page: pick actor + table + template, spawn a view, bind it, send events |
| `scripts/ui-e2e.sh` | real daemon: spawn counter, spawn view on it, subscribe over WS, 3 sends, assert 3 keyed deltas; promote a second template, assert one delta touching every key; prints `N/4` |
| `docs/architecture.md` | invariant 17; §8 gains "tree diffing" and "CRDTs" rows |
| `docs/future/repl-ui-actors.md`, `turso-cdc-and-views.md`, `README.md` | rows point here; removed when the goal commands print their numbers |
| `docs/future/browser-node.md` (new) | phase 2 with its `cargo check` command |

## Tests (the goal numbers)

`crates/loom-actor/tests/view.rs`:

1. `cdc_replay_reproduces_tables`: run `counter-v1` for 10 messages; apply the
   `turso_cdc` rows onto a copy of the cursor-0 snapshot; per-table blake3 hashes
   equal the live file's. Negative control: drop one CDC row, hashes differ.
2. `cdc_compacts_at_snapshot_and_floor_is_recorded`: after `snapshot_every`
   messages, `turso_cdc` has no rows below `cdc_floor`; a subscriber behind the floor
   receives `resnapshot` then a full snapshot, never a partial delta.
3. `ephemeral_leaves_no_file_no_object_no_lease`: spawn ephemeral under a node with
   `StoreConfig::Local`; directory listing and store prefix are unchanged; node
   restart forgets it; the parent's `children` row remains.
4. `ephemeral_supports_fork_and_validate`: `validate` on an ephemeral actor returns
   `Matched`.
5. `subscribe_snapshot_then_deltas_in_order`: subscriber gets `snapshot` (tagged
   change_id) then one `delta` per source commit, `change_id` strictly increasing,
   each frame carrying the source `seq` and inbox `key`.
6. `subscribe_requires_inspect`: a cap without INSPECT traps with the existing
   invalid-authority dead letter; nothing is inserted in `subscribers`.
7. `view_renders_keyed_trees_and_promote_rerenders_in_place`: view over `counter`
   with template A; 3 messages give 3 `tree` rows; promote template B; one
   transaction updates all 3 rows; `tree` deltas carry every key; the view's
   `code_changes` has both hashes.
8. `view_refuses_effectful_template`: a template with effect row `["sleep"]` is
   refused at spawn and at promote, naming `sleep`.
9. `subscription_crosses_pump_redelivery_once`: crash the subscriber after the
   delta is inserted but before the source marks delivered; redelivery leaves one
   inbox row (key `delta:<source>:<seq>:<sub>`).

`ui/tests/bind/`:

1. `insert_update_delete_reorder_keep_node_identity`: the `Element` object for a key
   is the same after an update and after a reorder; removed on delete.
2. `patch_touches_only_changed_attrs_and_text`: a mutation observer records exactly
   the changed attribute/text mutations, nothing else.
3. `pending_is_cleared_by_matching_key_and_reverted_by_dead_letter`.
4. `focus_and_caret_survive_promote`: an `<input>` inside a keyed row keeps
   `document.activeElement` and `selectionStart` across a delta that updates every
   key.
5. `resnapshot_rebuilds_without_losing_focused_row`: rows present in both snapshots
   keep their nodes.
