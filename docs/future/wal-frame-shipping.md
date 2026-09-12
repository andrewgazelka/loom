# WAL-frame shipping

Today history v1 ships `VACUUM INTO` snapshots plus logical segments (inbox, effects, outbox, code_changes rows) and rebuilds a fork by replaying the window with recorded effects (docs/actors-turso.md, Addendum C). Frame-level shipping would make `fork(id, seq)` a byte operation: latest snapshot plus WAL frames up to the frame recorded for `seq`.

Blocked on: `turso::Connection` (0.7.2) does not expose `wal_get_frame` / `wal_insert_frame` / `wal_changed_pages_after`; `turso_core::Connection` does (connection.rs ~2090-2150). Options: depend on `turso_core` directly for the frame path, or upstream an accessor.

Done when: `fork(id, seq)` reads no inbox rows; the five durability tests and the memo tests pass unchanged; segment size per message drops (measure).
