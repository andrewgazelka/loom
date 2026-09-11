# Reuse a handler by its definition hash

A stored Rust definition can export an ordinary public handler function alongside
its `#[loom::def]` entrypoint:

```rust
#[loom::def(effects = [])]
pub fn main() {}

pub fn handle(effect: loom::Effect, _continuation: loom::Continuation) -> loom::Reply {
    if effect.name == "sleep" {
        loom::Reply::Resume(loom::Value::Null)
    } else {
        loom::Reply::Forward
    }
}
```

Use the returned definition hash as a literal in another definition:

```rust
loom::handle_with("<64-character definition hash>", || {
    // Effects here run under the stored handler.
})
```

The caller declares the residual host-effect row explicitly when a stored
handler's callback cannot be inferred. The checker conservatively marks such a
callback unknown; its entrypoint's row is not a substitute for the callback row.

The checker resolves this syntax into an ordinary `loom::handle_any` call to the
pinned dependency's `pub fn handle`. The dependency edge is part of the caller's
content identity. Changing the pin creates a different caller definition;
existing callers retain their old handler. A missing hash fails definition
building. Rust checks the handler's complete function signature against the SDK.

This is definition-time linking, not a runtime plugin loader. The hash must be a
literal. The linked handler shares the execution memory and SDK types of its
caller, and uses the same handler ABI and safe-source admission. A dynamic hash
is rejected explicitly. Effect-row analysis still needs to account for effects
performed by the handler itself in its outer context; storing a handler does not
grant additional host effects or establish a proof of compiler soundness.

Run `bun scripts/bench/effects-handlers.ts` to check stored-handler identity,
missing-pin refusal, and recording/replay through a linked handler alongside
the handler semantics and round-trip benchmark.
