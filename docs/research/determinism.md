# Determinism is not a runtime promise

## What the original note used determinism for

1. Crash recovery mid-message by replay with recorded effects.
2. Validating candidate code by replaying recorded effects.
3. Bit-for-bit reproduction of past runs.

## Why it is dropped once state lives in SQLite

1. One message = one transaction; a crash rolls back and re-runs one message. External effects
   are idempotent by key `(actor_id, seq, effect_index)`. No replay, no gated imports.
2. Serving recorded effects by index only works when the candidate performs the same effects in
   the same order, which real changes break. Divergence is a verdict (`DivergedAt`), fitness
   is the caller's SQL assertions on the fork; a handler intercepts effects during validation.
3. History (snapshots + inbox + effects) still allows re-running any window against any code
   hash; only byte-identical re-execution is given up.

## What determinism would have cost

A deterministic fiber scheduler over shared-memory scoped children, gated imports,
synthesized time/random, and a rule that candidates preserve effect order (which fights
evolution).

## Kept because free

NaN canonicalization, relaxed SIMD off, stack/fuel/memory limits inside `toolchain_hash`
(traps become classifiable), random seeded per message from `(actor_id, seq, counter)`.
Pure definitions (no host effects) are deterministic by construction; replay is a
per-definition property.

## Prior art (read today)

Temporal and Restate document the same rule from the other side: non-deterministic operations
must be journaled or the replay diverges; durable execution re-executes control flow and skips
recorded side effects (jack-vanlightly.com 2025-11-24 "Demystifying Determinism in Durable
Execution").
