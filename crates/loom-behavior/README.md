# Loom definitions as behaviors

`StoreRegistry::new(store)` resolves stored Rust and JavaScript definitions by name or hash
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

## JavaScript actors

JavaScript definitions run in V8 isolates and share the same actor SQL,
capability checks, transactional outbox, and rollback behavior as Rust guests.
Use `loom.messages.json` to decode incoming JSON messages, including Unicode:

```javascript
const LOOM_SCHEMA = 'CREATE TABLE increments(amount INTEGER);';
const main = loom.messages.json(async message => {
  if (message.type === 'forward') {
    const peer = await loom.actors.accept(message.peer);
    await peer.send({type: 'increment', amount: 2});
  } else if (message.type === 'increment') {
    await loom.sql('INSERT INTO increments(amount) VALUES (?)', [message.amount]);
  }
});
```

A peer is a capability-bearing actor reference. `peer.send(value)` encodes JSON;
`peer.toJSON()` returns frozen capability bytes for handing that reference to
another actor, which must accept it before use. Throwing from the handler rolls
back its SQL writes and pending sends together. A plain `main(messageBytes)`
receives binary inbox messages; `peer.sendBytes(bytes)` sends them unchanged.
The lower-level `loom.perform` descriptor API
remains available for all actor operations listed below.

`LoomBehavior::from_sandbox(hash, schema, Arc<dyn Sandbox>)` accepts another
engine or a wrapper around an existing sandbox. The sandbox receives the same
borrowed actor effect handler. `StoreRegistry::with_v8(store, engine)` shares a
V8 engine across registries.

## View templates

`StoreRegistry::template(reference)` resolves a `LoomTemplate` implementing
`loom_actor::Template`. A template has exactly one `render(row: loom::Value) ->
loom::Value` entry. Both its definition and entry effect rows must be known and
empty; admission names the first forbidden effect or the `unknown` flag.

The native `view-v1` behavior calls this bridge inside its existing actor
transaction. `LoomTemplate::render` uses `Runtime::call_with_effects` with one
positional Value argument and returns its decoded Value. The call-local root
handler refuses every escaped effect, including effects omitted by malformed
metadata. No actor is created in `loom-store`, and runtime failures retain the
actor retry classification. This is the added Value entry point; ordinary
`LoomBehavior` keeps its byte-message entry point.

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
| `actor.resolve` | `{name:string}` | SEND-only capability bytes for a published name |
| `actor.sender_cap` | null | capability bytes supplied with the incoming driver message |
| `actor.promote` | `{cap, behavior_hash, author, rationale}` | null |
| `actor.inspect_sql` | `{cap, sql, params}` | `{columns, rows}` |
| `actor.send` | `{cap, msg}` | null |
| `actor.subscribe` | `{cap, table}` | subscription ID (requires INSPECT) |
| `actor.unsubscribe` | `{subscription_id}` | null |
| `actor.spawn` | serialized `loom_actor::ChildSpec` | capability bytes |
| `actor.spawn_driver` | `{hash:string, init:bytes}` | capability bytes for the driver |
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

An administrator publishes actor names with `Node::register`. `actor.resolve`
resolves only those names and issues a SEND-only grant within the tenant.
An arbitrary actor ID does not confer authority. Guests cannot publish names
through this effect API.

`actor.spawn_driver` selects a registered native driver by hash and queues its
creation in the transaction's outbox. The driver opens resources after commit.
`actor.sender_cap` retrieves the authority supplied with an incoming driver
message and persists the token for later turns. It traps when the message has
no supplied sender authority; ordinary actor messages carry explicit reply
capabilities in their payloads.

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
