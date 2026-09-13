# Turso CDC and incremental views

Design: [docs/ui-view-actor.md](../ui-view-actor.md) (2026-09-12) makes CDC the delta source for subscriptions and views; invariant 17 there is this row's done-when.

Turso's change data capture (`PRAGMA capture_data_changes_conn`, stable since 0.5) gives a per-actor logical change log; DBSP materialized views maintain derived tables incrementally. The validator's logical diff could read `turso_cdc`; the doc's derived tables could be views.

The authored validator now cross-checks table hashes with CDC replay; `view-v1` materializes `tree` rows through the same subscription pump. The write-only lane has not executed its gates. Keep this entry until `cargo test -p loom-actor --test view` reports nine passing tests and `scripts/ui-e2e.sh` reports 4/4. The implementation and deviations are recorded in [the actor runbook](../../crates/loom-actor/RUNBOOK.md#view-actors-and-cdc).
