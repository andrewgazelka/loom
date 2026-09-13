# REPL UI for actors

Design: [docs/ui-view-actor.md](../ui-view-actor.md) (2026-09-12): the DOM as a materialized view of actor tables via Turso CDC deltas and a `view-v1` actor.

The old actor panel was removed with the event-fold model. The UI should drive actors through the same MCP surface: tree, info, inbox and outbox, lineage, dead letters, validate verdicts, promote.

Done when: every panel's data comes from an `actor_*` tool or `actor://` resource (no second API), keyboard-driven per the UI rules, verified with a headless run.
