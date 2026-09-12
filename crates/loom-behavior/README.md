# Loom definitions as behaviors

`StoreRegistry::new(store)` resolves stored Rust definitions by name or hash
when an actor is spawned or promoted. Pass it in an `Arc` to
`loom_actor::Node::new`. Definitions added to the same store become available
without restarting the node; actors retain their resolved definition hash.

Each message invokes a root-level `pub fn` function with one `Vec<u8>` argument.
The bytes are unchanged, including non-UTF-8 messages. The function's return
value is discarded; durable state belongs in the actor's SQL tables. Guest
handlers and scoped tasks retain their normal Loom semantics. Only effects
that escape guest handlers reach the actor context.

```rust,ignore
pub fn message(msg: Vec<u8>) {
    let mut request: loom::Value = loom::serde_json::from_str(r#"{
        "sql":"INSERT INTO messages(body) VALUES (?)",
        "params":[{"type":"blob","value":null}]
    }"#).unwrap();
    request["params"][0]["value"] =
        loom::Value::Array(msg.into_iter().map(loom::Value::from).collect());
    loom::perform::<loom::Value>("sql", request).unwrap();
}
```

Schema uses `pub const LOOM_SCHEMA: &str`. The driver evaluates the constant and reports its SQL in the JSON `schema` field. The generated
`loom_schema() -> u64` core export returns the ordinary Loom CBOR result
envelope containing SQL text. Behavior resolution invokes it in a pure execution;
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
| `actor.accept` | `{cap}` | null |
| `actor.cap` | `{cap_id:decimal_string}` | previously held capability bytes |
| `actor.attenuate` | `{cap, rights:{bits}}` | attenuated capability bytes |
| `actor.revoke` | `{cap_id:decimal_string}` | null |
| `actor.self_cap` | null | capability bytes for the executing actor |
| `actor.promote` | `{cap, behavior_hash, author, rationale}` | null |
| `actor.inspect_sql` | `{cap, sql, params}` | `{columns, rows}` |
| `actor.send` | `{cap, msg}` | null |
| `actor.spawn` | serialized `loom_actor::ChildSpec` | capability bytes |
| `actor.stop` | `{cap, reason}` | null |
| `actor.monitor` | `{cap}` | monitor reference |
| `actor.demonitor` | `{reference, flush}` | null |
| `actor.link`, `actor.unlink`, `actor.shutdown` | `{cap}` | null |
| `actor.exit` | `{reason}` | null |
| `actor.trap_exit` | `{enabled}` | null |
| `actor.restart` | `{cap, verb}` | null |
| `actor.inspect` | `{cap}` | serialized `ChildState` |
| `actor.send_after` | `{cap, ms, msg}` | timer reference |
| `actor.cancel_timer` | `{reference}` | null |
| `actor.read_timer` | `{reference}` | remaining milliseconds or null |
| `actor.call` | `{cap, msg, timeout_ms}` | call reference |
| `actor.reply` | `{cap, reference, msg}` | null |
| `actor.seq`, `actor.self_id`, `actor.sender` | null | sequence, ID, optional sender |
| `actor.defer` | null | null |
| `now` | null | recorded Unix milliseconds |
| `random` | `{n}` | `n` deterministic bytes |
| host effect name | `{request, mode?}` | bytes for short; request ID for request |

Every `cap` field is an array of bytes containing the UTF-8 JSON serialization
of `loom_actor::Cap`: `{target, cap_id, epoch, rights:{bits}, mac}`, with a
32-byte MAC array. The outer descriptor remains Loom CBOR. Keeping the complete
token in bytes preserves unsigned 64-bit IDs and epochs across wire encodings.
The standalone `cap_id` in `actor.cap` and `actor.revoke` is a decimal string: Loom
descriptor numbers are limited to exactly representable 53-bit integers.
The bridge decodes the token and passes it to the corresponding `Ctx` method;
that host path verifies its MAC, epoch, revocation, and required rights. Decoding
never grants authority, resolves an ID into a cap, or calls `Node::cap_for`.
Missing caps and old `target`/`from` ID fields are rejected.

A guest obtains a cap from a message payload or `actor.spawn`. It calls
`actor.accept` on a received token to verify and persist it. Later turns can
retrieve that held token with `actor.cap`; knowing another actor's ID or
`actor.sender` does not permit sending. An operator provisions a message with
`serde_json::to_vec(&node.cap_for(id, rights).await?)?` as its `cap` field.
Spawn and attenuation results use the same token-byte representation.

The SDK's `loom::actor::Cap { token: Vec<u8> }` serializes transparently to these
bytes. Its helpers construct the same descriptors:

```rust,ignore
let payload: loom::Value = loom::serde_json::from_slice(&msg).unwrap();
let handed: loom::actor::Cap =
    loom::serde_json::from_value(payload["cap"].clone()).unwrap();
let cap = loom::actor::accept(handed).unwrap();
loom::actor::send(&cap, b"hello").unwrap();
```

The bridge preserves a rejected `Ctx` operation's deterministic `Trap`, including
the operation and `cap_id`; swallowing the effect error cannot commit the turn.
Validation uses the same recorded capability interception as native behaviors,
without granting the fork access to live actors.

SQL cells are tagged values: `{type:"null"}` or
`{type:"integer"|"real"|"text"|"blob", value:...}`. Rows use the same cells
in column order. Reals must be finite. `actor.spawn` uses `behavior_hash`;
`"$self"` resolves to the executing definition's hash. Child policy defaults
are owned by `ChildSpec`.

Host effect `mode` is `"short"` by default. `"request"` selects the actor's
two-phase outbox path; completion arrives as a later inbox message. For example:

```rust,ignore
let request: loom::Value = loom::serde_json::from_str(
    r#"{"request":[104,105],"mode":"request"}"#,
).unwrap();
let request_id: String = loom::perform("echo", request).unwrap();
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

`cargo test -p loom-behavior --test behavior` runs ten behavior tests, including
`guest_without_cap_cannot_send`. The send test covers both accepted message
capabilities and a spawn-result capability.
The `loom-behavior-fixtures` helper uses `loom_build::Builder` for preparation
and compilation, including shared-memory transformation and protocol stamping.
A missing compiler or a failed build fails the tests. Fixtures compile once per test
process; actor databases are separate temporary directories.
