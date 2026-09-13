# Ingress batching

Measured 2026-09-12 on hydra (debug build, `StoreConfig::Local` directory, `ship_interval` 20 ms, `crates/loom-actor/tests/cluster.rs::two_nodes_pair_fifo`): one cross-node message costs a sender-side ship of 60-100 ms (output gate) plus a forward of 120-390 ms, of which the receiver's ack waits 80-250 ms for its own ship to cover the inbox row. Serial per (sender, target) pair, so 100 messages drain in 75-105 s. The pump forwards ONE outbox row per HTTP call (`crates/loom-actor/src/ingress_transport.rs::route_outbox`).

The ingress contract already accepts a batch (`IngressRequest { ops }` applied in order, `applied` = successful prefix). Batching every undelivered row for one destination into one forward amortizes both ships: one sender ship per batch, one receiver ship per batch.

Done when: the same test drains 100 messages in under 10 s on hydra (debug) with per-pair FIFO and exactly-once unchanged, and `docs/multi-node.md` records the new p50/p99 beside the numbers above.
