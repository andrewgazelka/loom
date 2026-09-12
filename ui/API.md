# REPL HTTP contract

This is the UI lane's contract for the API lane. All 27 operations use POST with
`Content-Type: application/json`. An empty request is `{}`. A configured bearer
token is sent as `Authorization: Bearer …`. The API enforces the same scopes as
MCP. Success is the **raw JSON value**, without a `result` or MCP text envelope.
Errors use a non-2xx status and a body naming the operation and affected identity.
The UI displays that body. It never retries a mutation automatically.

Definition routes are `/api/definitions/<operation>`.
Actor routes retain the complete MCP name: `/api/actors/actor_<operation>`.
For example, `/api/actors/actor_validate`, not `/api/actors/validate`.
`src/lib/workbench/commands.ts` owns command names, forms, and table queries.
`schema.ts` and `client.ts` validate results from both live and fixture transports.

## Definitions

| Operation    | Request                                                       | Success                                          |
| ------------ | ------------------------------------------------------------- | ------------------------------------------------ |
| `find`       | `{query: string}`; empty query lists all                      | `Definition[]`                                   |
| `view`       | `{hash: string}`                                              | `DefinitionView`                                 |
| `add`        | `{name, source, deps: Record<string,string>}`                 | `DefinitionView`                                 |
| `update`     | `{name, expected_hash, source, deps?: Record<string,string>}` | `DefinitionView`                                 |
| `history`    | `{name: string}`                                              | `Revision[]`, newest first                       |
| `diff`       | `{before: string, after: string}`                             | `DefinitionDiff`                                 |
| `run`        | `{hash: string, args: JSON[]}`                                | `{output: JSON, effects: Record<string,JSON>[]}` |
| `dependents` | `{hash: string}`                                              | `Definition[]`                                   |

All source is Rust. `update` compares `expected_hash` atomically with the named
head and rejects a conflict. Omitted `deps` preserves dependencies; `{}` explicitly
clears them. Opening update reads source through `view`. `run.effects` contains
performed effects, not just declarations. Entries should name the effect kind,
request, result, and completion status. Errors during a run remain API errors.

```ts
interface Definition {
  name: string;
  hash: string; // checked definition identity
  entry_item_hash: string | null; // null explicitly means unavailable
  updated: string; // ISO 8601 timestamp
}
interface DefinitionView extends Definition {
  source: string;
  items: Item[]; // [] explicitly means no item metadata
}
interface Item {
  name: string;
  hash: string;
  preimage_size: number; // nonnegative integer, bytes
  refs: string[];
}
interface ChangedItem {
  name: string;
  before: string | null; // null for an added item
  after: string | null; // null for a removed item
}
interface Revision {
  hash: string;
  parent_hash: string | null;
  updated: string;
  changed_items: ChangedItem[];
}
interface DefinitionDiff {
  before: string;
  after: string;
  source_before: string;
  source_after: string;
  changed_items: ChangedItem[];
}
```

Definition identity and entry item identity are displayed separately. The UI does
not infer item identities from source, Wasm, or definition hashes. Preimage size
is the actual canonical preimage byte count; a cyclic member can therefore be 40
bytes. The UI compares returned source lines only for display. Changed-item
identity comparisons come from the server.

## Actors

Requests and responses match the JSON serialized by
`crates/loom-mcp/src/actors.rs`. Optional fields are omitted, never sent as blank
strings. Initialization `null` is preserved. Every integer must fit JavaScript's
safe integer range; larger values fail with a named boundary error.

| Operation             | Request                                                       | Success                                     |
| --------------------- | ------------------------------------------------------------- | ------------------------------------------- | --------- | -------------- |
| `actor_list`          | `{}`                                                          | `Actor[]`                                   |
| `actor_tree`          | `{root?: string}`                                             | `ActorNode`                                 |
| `actor_info`          | `{id}`                                                        | Actor lifecycle fields below; no `id` field |
| `actor_send`          | `{id, msg: JSON, key?: string}`                               | `{cursor: number}`                          |
| `actor_spawn`         | `{behavior_hash, init: JSON, parent?: string, spec?: object}` | `{id: string}`                              |
| `actor_stop`          | `{id, reason}`                                                | `{id: string}`                              |
| `actor_restart`       | `{id, verb: "resume"                                          | "skip"                                      | "reset"}` | `{id: string}` |
| `actor_promote`       | `{id, behavior_hash, author, rationale}`                      | New code-change row                         |
| `actor_promote_where` | `{old_hash, new_hash, author, rationale}`                     | `string[]` actor ids                        |
| `actor_lineage`       | `{id}`                                                        | Code-change rows                            |
| `actor_dead_letters`  | `{id}`                                                        | Dead-letter rows                            |
| `actor_fork`          | `{id, at_seq: number}`                                        | `{id: string}`                              |
| `actor_validate`      | `{id, candidate_hash, k: number, assertions?: string[]}`      | `ValidationResult` below                    |
| `actor_sql`           | `{id, query, params?: JSON[]}`                                | Object rows, keyed by column name           |
| `actor_whereis`       | `{name}`                                                      | Actor id string or null                     |
| `actor_register`      | `{name, id}`                                                  | `{id, name}`                                |
| `actor_members`       | `{group}`                                                     | `string[]` actor ids                        |
| `actor_behaviors`     | `{}`                                                          | `{hash, description}[]`                     |
| `actor_run`           | `{}`                                                          | `{processed: number}`                       |

`Actor` has `id`, `status`, `behavior_hash`, `cursor`, `inbox_len`, and nullable
`parent`. `ActorNode` has `id`, `status`, `behavior_hash`, `cursor`, and nested
`children: ActorNode[]`. The explorer joins `actor_list.inbox_len` by id; the tree
response does not invent an inbox count. Missing list identities fail explicitly.
`actor_info` returns the same lifecycle fields without `id`, plus `reason`,
`deferred_len`, `links`, `monitors`, and `children`. Its panel heading uses the
submitted id. `reason` is a string, empty when no reason is recorded. Status
labels are verbatim; status dots vary luminance.

The three table views call **the same actor_sql operation**:

```sql
SELECT * FROM inbox ORDER BY seq
SELECT * FROM outbox ORDER BY seq,idx
SELECT * FROM effects ORDER BY seq,idx
```

They need no new HTTP resource routes. SQL rows retain JSON byte arrays for BLOB
columns, matching MCP. Opening a row displays its complete JSON. SQL parameters
are scalars. The server owns read-only SQL admission.

## Validation

The wire format uses Rust's externally tagged enum, with named table records:

```ts
type Verdict =
  | { Matched: { tables: { name: string; hash: string }[] } }
  | {
      Differs: {
        tables: {
          name: string;
          original_hash: string;
          fork_hash: string;
        }[];
      };
    }
  | {
      DivergedAt: {
        seq: number;
        idx: number;
        expected: number[];
        got: number[];
      };
    }
  | { Trapped: { seq: number; error: string } };
interface ValidationResult {
  verdict: Verdict;
  assertions: { query: string; passed: boolean }[];
}
```

`DivergedAt` bytes remain exact byte arrays. This verdict has no table hashes,
and the panel says so. `Matched` and `Differs` display all named table hashes.
Assertion failures remain distinct from the replay verdict. Validation never
promotes; promotion is a separate explicit operation.

## Fixture transport

`?mock=1` explicitly selects `fixtures.json` through `MockTransport`. It does not
contact a daemon. The header and footer identify the static fixture preview.
Mutation results are fixed snapshots; they do not simulate persistence. Any
panel can be opened with `&panel=<command id>`. Validation fixtures select one of
`&verdict=Matched`, `DivergedAt`, `Differs`, or `Trapped`. All fixture responses
pass through the same response parsers as HTTP responses.

## Active build progress

`GET /v1/builds/active` requires bearer authentication and Read access. It returns
the standard response envelope with `result.active` either null or
`{name, stage, elapsed_ms}`. Stages are `preflight`, `check`, `compile`, and
`publish`. Completion, failure, and cancellation clear the active build.
The endpoint observes the node's serialized definition intake; it does not
submit, retry, or cancel a command. Build output remains in the completed
publication's `build.logs_ref` and is retrieved through authenticated CAS reads.
