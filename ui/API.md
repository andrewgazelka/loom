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
| `view` | `{target}` | Source, item hashes, executable identities and entry effects |
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

## Build inspection

`GET /v1/builds/active` reports the active build stage. Compiler logs are fetched
from `GET /v1/cas/<logs_ref>`. Both requests carry the configured bearer token.

For retry-safe initial submissions, pass a caller-chosen `request_id` to `update`.
That ID is also the session ID for `update_view`; replaying identical inputs
returns the existing session, and reusing the ID for different inputs is rejected.
Source-only repairs preserve previously supplied dependency and effect-policy edits.
