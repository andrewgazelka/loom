# Loom definitions as behaviors

`register(&mut registry, store, def_hash).await?` validates a stored Rust
definition, reads its schema, and registers a `LoomBehavior` under `defs.hash`.
Pass that registry to `loom_actor::Node::new`. The same hash is the
`behavior_hash` accepted by actor spawning and promotion.

Each message invokes a `#[loom::def]` function with one `Vec<u8>` argument.
The bytes are unchanged, including non-UTF-8 messages. The function's return
value is discarded; durable state belongs in the actor's SQL tables. Guest
handlers and scoped tasks retain their normal Loom semantics. Only effects
that escape guest handlers reach the actor context.

```rust,ignore
#[loom::def]
pub fn message(msg: Vec<u8>) {
    loom::perform::<loom::Value>("sql", loom::serde_json::json!({
        "sql": "INSERT INTO messages(body) VALUES (?)",
        "params": [{"type":"blob", "value":msg}]
    })).unwrap();
}
```

Schema uses `#[loom::schema] pub fn schema() -> &'static str`. Its generated
`loom_schema() -> u64` core export returns the ordinary Loom CBOR result
envelope containing SQL text. Registration invokes it in a pure execution;
an absent export means empty SQL. The actor runtime runs that SQL during
creation and promotion. Schema extraction does not create a legacy actor or
invoke `run`, `fold`, `spawn`, `send`, or `state` on `loom_rt::Runtime`.

## Effect wire contract

Descriptors use the existing `{ "op": name, "args": arguments }` Loom CBOR
format, constructed by `loom::perform`. Byte vectors are arrays of unsigned
octets. Unit responses are null. Undeclared fields and malformed arguments
trap with the operation's name.

| Operation | Arguments | Result |
| --- | --- | --- |
| `sql` | `{sql, params}` | `{columns, rows}` |
| `actor.send` | `{target, msg}` | null |
| `actor.spawn` | serialized `loom_actor::ChildSpec` | child ID |
| `actor.stop` | `{target, reason}` | null |
| `actor.monitor` | `{target}` | monitor reference |
| `actor.demonitor` | `{reference, flush}` | null |
| `actor.link`, `actor.unlink`, `actor.shutdown` | `{target}` | null |
| `actor.exit` | `{reason}` | null |
| `actor.trap_exit` | `{enabled}` | null |
| `actor.restart` | `{target, verb}` | null |
| `actor.inspect` | `{target}` | serialized `ChildState` |
| `actor.send_after` | `{target, ms, msg}` | timer reference |
| `actor.cancel_timer` | `{reference}` | null |
| `actor.read_timer` | `{reference}` | remaining milliseconds or null |
| `actor.call` | `{target, msg, timeout_ms}` | call reference |
| `actor.reply` | `{from, reference, msg}` | null |
| `actor.seq`, `actor.self_id`, `actor.sender` | null | sequence, ID, optional sender |
| `actor.defer` | null | null |
| `now` | null | recorded Unix milliseconds |
| `random` | `{n}` | `n` deterministic bytes |
| host effect name | `{request, mode?}` | bytes for short; request ID for request |

SQL cells are tagged values: `{type:"null"}` or
`{type:"integer"|"real"|"text"|"blob", value:...}`. Rows use the same cells
in column order. Reals must be finite. `actor.spawn` uses `behavior_hash`;
`"$self"` resolves to the executing definition's hash. Child policy defaults
are owned by `ChildSpec`.

Host effect `mode` is `"short"` by default. `"request"` selects the actor's
two-phase outbox path; completion arrives as a later inbox message. For example:

```rust,ignore
let request_id: String = loom::perform("echo", loom::serde_json::json!({
    "request": [104, 105], "mode": "request"
})).unwrap();
```

Host handlers consume and return opaque bytes. A short effect is called through
`Ctx::effect`; a two-phase effect is enqueued through `Ctx::request`. The node's
`EffectHandler` owns support for host effect names. Unknown actor operations
and rejected host effects trap; errors cannot be swallowed by guest code to
commit a partially failed message. A host effect requested in two-phase mode
is resolved by the pump after commit, as required by `Ctx::request`.

Guest failures are deterministic `Trap`s. Errors returned by `Ctx` retain their
classification. Other runtime failures use the actor runtime's retry path.
The call-local bridge bypasses the old store's actor scheduling, memoization,
and trace persistence. Dropping a call cancels its guest execution; no background
task owns or retains the borrowed `Ctx`.

## Verification

`cargo test -p loom-behavior --test behavior` runs the four acceptance tests.
The `loom-behavior-fixtures` helper uses `loom_build::Builder` for preparation
and compilation, including shared-memory transformation and protocol stamping.
A missing compiler or a failed build fails the tests. Fixtures compile once per test
process; actor databases are separate temporary directories.
