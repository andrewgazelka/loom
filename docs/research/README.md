# Research notes

Working notes behind the design decisions in this repository, kept so the reasoning can be
reused and checked. Each note says what was read (with the date) and what was recalled from
model memory (labeled). A claim without a source is a claim about the author's memory.

| Note | Decision it backs |
|---|---|
| [content-addressing.md](content-addressing.md) | Unison-style per-item hashes from a rustc driver; HIR not MIR; the artifact chain |
| [compilation-caching.md](compilation-caching.md) | rustc incremental per lineage; wasmtime per-function cache; codegen-backend object cache |
| [sandboxing.md](sandboxing.md) | wasm stays the guest artifact and sandbox; SFI-on-Cranelift is an option with a trigger, not a plan |
| [storage-engine.md](storage-engine.md) | Turso 0.7.2, one file per actor, over SQLite/libsql/Postgres |
| [durability-and-failover.md](durability-and-failover.md) | object-store shipping, leases by conditional PUT, no Raft |
| [determinism.md](determinism.md) | determinism is not a runtime promise; idempotent keyed effects instead |
| [actor-model.md](actor-model.md) | one message = one transaction, outbox after commit, OTP parity table |

Dates are 2026-09-11 unless stated. "Read" means fetched and read that day (Exa search or
the crate source in `~/.cargo/registry`); "recalled" means model memory, unverified.
