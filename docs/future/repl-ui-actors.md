# REPL UI for actors

Design: [docs/ui-view-actor.md](../ui-view-actor.md) (2026-09-12): the DOM as a materialized view of actor tables via Turso CDC deltas and a `view-v1` actor.

The old actor panel was removed with the event-fold model. The UI should drive actors through the same MCP surface: tree, info, inbox and outbox, lineage, dead letters, validate verdicts, promote.

The server view actor and plain TypeScript binding implementation are authored in the `r5-view` lane. Its unexecuted gates are `cargo test -p loom-actor --test view`, `bun test ui/tests/bind`, and `scripts/ui-e2e.sh`. Keep this roadmap entry until those commands report 9/9, 5/5, and 4/4.
