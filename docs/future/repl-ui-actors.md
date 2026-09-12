# REPL UI for actors

The old actor panel was removed with the event-fold model. The UI should drive actors through the same MCP surface: tree, info, inbox and outbox, lineage, dead letters, validate verdicts, promote.

Done when: every panel's data comes from an `actor_*` tool or `actor://` resource (no second API), keyboard-driven per the UI rules, verified with a headless run.
