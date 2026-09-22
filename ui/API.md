# REPL HTTP contract

Every command uses `POST /v1/command` with `Content-Type: application/json` and
`{"command":"<verb>","args":{...}}`. A configured bearer token is sent as
`Authorization: Bearer …`. Responses use the shared protocol envelope:

```json
{"ok":true,"seq":1,"result":{},"diagnostics":[]}
```

`ok:false` carries the operation error in `result`. HTTP failures also display
status and response text. The UI does not retry mutations automatically.

The Rust verb table in `crates/loom-proto/src/verbs.rs` owns the wire vocabulary.
`src/lib/workbench/commands.ts` supplies matching forms; the parity test checks
names, fields, optionality and defaults. `schema.ts` and `client.ts` validate
live and fixture responses at the same boundary.

## Definitions

| Command | Arguments | Result |
| --- | --- | --- |
| `find` | `{text}` | Definitions with names, hashes and item hashes |
| `view` | `{target}` | Source (stored and rustfmt-formatted), item hashes, executable identities and entry effects |
| `add` | `{name?,source,deps?,allowed_effects?}` | Published definition view |
| `update` | `{name,source,expected_hash?,deps?,allowed_effects?}` | Update session, plus definition view when complete |
| `update_view` | `{id}` | Current update session, plus view when complete |
| `update_repair` | `{id,revision,changes}` | Next update session, plus view when complete |
| `update_rebase` | `{id,revision}` | Retry a conflicted update against the current namespace |
| `update_abort` | `{id,revision}` | Aborted update session |
| `history` | `{name}` | Revision history and item changes |
| `diff` | `{old,new}` | Added, removed and changed items |
| `run` | `{target,args?}` | Output JSON and performed effects |
| `dependents` | `{hash}` | Dependent definition hashes |

A `view` result carries the stored bytes as `source` and, for Rust definitions, a rustfmt
rendering as `formatted_source` (`string | null`) with `format_error` (`string | null`) naming
why formatting failed when it is null. The UI shows `formatted_source` by default and the stored
bytes only behind "as submitted"; a null rendering shows the stored bytes with the error as a
one-line note, and a response without the fields says so in that note. Both fields being
non-null is rejected.

Opening `update` loads source and its `expected_hash` together through `view`.
That guard detects an agent changing the definition while a human edits it.
A complete publication advances the form's guard to the newly published hash.
Omitted dependencies preserve existing dependencies; an empty object clears them.

## Repair sessions

An update response can contain only `update`, without a definition view. Status
is `pending`, `needs_repair`, `conflict`, `complete` or `aborted`. Sessions expose
an `id`, revision, target, expected namespace, saved edits, propagated hash
changes and diagnostics. Each diagnostic includes affected names, the old hash,
source, compiler diagnostics and optional build metadata.

`Repair sources` opens a JSON editor prefilled from affected sources:

```json
{
  "id": "session-id",
  "revision": 4,
  "changes": {
    "caller": {
      "source": "pub fn caller() -> i64 { 42 }"
    }
  }
}
```

Repairs can also specify `deps` and `allowed_effects` per definition. An unnamed
definition uses its hash as the key. The revision guards concurrent repairs by
agents and humans. A returned revision advances the form before another repair.
Names move together only after the affected graph compiles and publication
checks the namespace snapshot. Old definitions retain their hashes.

A conflict exposes `Rebase update`. Rebase adopts current namespace changes only
when explicitly edited definitions are unchanged. Otherwise the source remains
available to reconcile in a new update. Abort opens a command form and requires
execution; it does not publish staged changes.

## Journal events and stream

`GET /v1/events?after=<seq>&limit=<n>` returns `{ok, seq, result: [{seq, ts, event}]}`; `limit`
is capped at 1000 and `ts` is Unix seconds. `GET /v1/stream` upgrades to a WebSocket: the
client sends `{"token": "<bearer>", "after": <seq>}`, the server answers `{"ok":true}` and then
pushes every later journal row as `{seq, ts, event}` text frames; `{"error": "..."}` reports a
failure. Actor table subscription frames share the socket and carry no `seq`.

Event shapes the board reads (`crates/loom-store/src/publication.rs`, `crates/loom-api/src/definitions.rs`,
`crates/loom-store/src/trace.rs`):

| `event.type` | Fields read |
| --- | --- |
| `defined` | `name`, `def.hash`, `def.lang`, `def.component_hash`, `def.sig.exports[].name`, `def.sig.effects.labels`, `deps` (alias to hash) |
| `component_built` | `component_hash`, `logs_ref`, `ms`, `size`, `rustc_invocations` |
| `call_completed`, `call_checkpoint` | `definition_hash`, `outcome` (`{status: success \| error \| cancelled, ...}`); `call_completed` also `elapsed_ms` when present |
| `actor_message` | `actor`, `definition_hash`, `cursor` |
| anything else | `type` only; shown as a feed row |

`tree` returns `{id, status, behavior_hash, cursor, children: [...]}` from the root supervisor;
`actors` returns `[{id, status, behavior_hash, cursor, inbox_len, parent}]`.

## Wasm inspection

`GET /v1/wasm/{component_hash}` (bearer token) returns the compiled component as text with its
DWARF line map, as a bare object (not the command envelope):

```json
{
  "debug": true,
  "wat": "(module\n  (type (;0;) (func))\n  ...",
  "functions": [{ "index": 0, "name": "loom_definition::counter::h1a2b", "exported": false, "start_line": 21, "end_line": 45 }],
  "lines": [{ "wat_line": 40, "file": "src/lib.rs", "line": 12 }]
}
```

`wat` is the whole module, one instruction per line; `functions[].name` is null for unnamed
functions and `start_line`/`end_line` are 1-based inclusive wat lines. `lines` maps wat lines to
source positions: `file` is `src/lib.rs` for the guest's own code and a sysroot or registry path
for anything else. `debug: false` means the artifact carries no DWARF and `lines` is empty. The
board fetches the module once per component hash and keeps it in memory for the session
(`src/lib/board/connect.ts`, `wasm()`); the shape is validated by `src/lib/board/wasm.ts`.

## Build inspection

`GET /v1/builds/active` reports the active build stage. Compiler logs are fetched
from `GET /v1/cas/<logs_ref>`. Both requests carry the configured bearer token. A
compiler log is JSON lines; the lines whose object has `build_stages` map stage names
to milliseconds (`crates/loom-build/src/stages.rs`), with `unattributed_ms` for the
remainder.

For retry-safe initial submissions, pass a caller-chosen `request_id` to `update`.
That ID is also the session ID for `update_view`; replaying identical inputs
returns the existing session, and reusing the ID for different inputs is rejected.
Source-only repairs preserve previously supplied dependency and effect-policy edits.
