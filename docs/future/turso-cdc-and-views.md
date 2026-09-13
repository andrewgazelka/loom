# Turso CDC and incremental views

Design: [docs/ui-view-actor.md](../ui-view-actor.md) (2026-09-12) makes CDC the delta source for subscriptions and views; invariant 17 there is this row's done-when.

Turso's change data capture (`PRAGMA capture_data_changes_conn`, stable since 0.5) gives a per-actor logical change log; DBSP materialized views maintain derived tables incrementally. The validator's logical diff could read `turso_cdc`; the doc's derived tables could be views.

Done when: table-hash comparison in `validate` is replaced or cross-checked by the CDC log; one derived table in a builtin behavior is a materialized view with a test that it updates on insert.
