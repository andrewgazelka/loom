# Reuse a handler by its definition hash

A stored Rust definition can export an ordinary public handler function alongside
its root public entrypoint:

```rust
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

The driver combines the pinned handler's stored residual row with the caller's body row. Labels covered by a total handler are removed from the body row; effects performed by the handler itself remain in the outer row. Runtime-selected effect labels are rejected with their call sites.

The checker resolves this syntax into an ordinary `loom::handle_pinned` call to the
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
