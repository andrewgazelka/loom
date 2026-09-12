# Cells and distribution

Ids reserve a namespace/cell prefix; delivery is single-node. Distribution means: a `send` to an id on another node goes through that node's ingress into the target's inbox with the same keyed exactly-once guarantee; `monitor_node`; lease takeover across machines (the object store already arbitrates); a small always-on ingress that wakes a dark node when a message arrives (scale to zero).

Done when: two nodes on two hosts exchange messages with per-pair FIFO preserved; killing the owner host moves an actor to the other host within the lease TTL; the ingress boots a cold node on demand.
