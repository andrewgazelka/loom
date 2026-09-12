# Population queries

Per-actor files make cross-actor queries a scan. A derived query store (Postgres or ClickHouse) fed by the segment stream answers lineage-across-population and operational questions; actors never read it, so losing it costs nothing.

Done when: `who runs hash H` and `lineage across all actors` are one query on the derived store, kept within one ship interval of the truth.
