# Storage engine for one-file-per-actor

## Candidates

| Engine | Verdict | Why |
|---|---|---|
| SQLite (rusqlite, bundled) | the safe default | most tested; one writer per file is the roofline for this shape; WAL + `synchronous=NORMAL` (no fsync per commit; sync at checkpoint; sqlite.org/pragma.html read today) |
| libsql | no | the C fork Turso is winding down; virtual WAL (`bottomless`) is mature but the old lane |
| Turso 0.7.2 (Rust rewrite) | chosen | 0.6 (May 2026) parity claim; 0.7 (Jul 2026) beta warning dropped, "runs in production at multiple organizations", not 1.0; MVCC beta; custom I/O trait; features the design uses later: CDC (`PRAGMA capture_data_changes_conn`, stable 0.5) and DBSP materialized views with incremental maintenance (read: turso.tech blog posts, docs.turso.tech compatibility page) |
| Postgres, one shared | no | one WAL per cluster: no per-actor fork, no cold tier, socket round trip per statement (violates the no-per-op-RPC rule) |
| Postgres, one per actor | no | a process group per actor: ~40 MB data dir, seconds to start, memory floor per instance (recalled) |
| PGlite (Postgres in wasm) | only if the dialect is the want | tens of MB per instance, single connection, JS-oriented (recalled) |

## Verified on hydra (turso 0.7.2 probe, /Volumes/Projects/tmp/turso-probe)

- `Builder::new_local(path).experimental_vacuum(true).build().await`; `PRAGMA journal_mode=WAL`
  returns a row so it must go through `query`, not `execute` (`execute` returns
  `Misuse("unexpected row during execution")` for any row-returning statement).
- `BEGIN`/`ROLLBACK`/`COMMIT` via `execute` behave (rolled-back insert absent, committed present).
- `VACUUM INTO '<path>'` works with the experimental flag: the snapshot mechanism for history v1.
- turso_core exposes `wal_get_frame`, `wal_insert_frame`, `wal_changed_pages_after`
  (connection.rs ~2090-2150): the seam for frame-level shipping later.
- I/O backends: `"memory"`, `"syscall"`, `"io_uring"` (Linux, feature `io_uring`), selected by
  `Builder::with_io(name)`.

## WAL shipping options

1. Own it in-process: `sqlite3_wal_hook` / Turso frame API after commit; segments to an object
   store; checkpoint only after upload (what Litestream does; a few hundred lines).
2. Litestream v0.5 (Oct 2025, LTX format, PITR via pragma, read-from-S3 VFS): a Go sidecar,
   not designed for thousands of files per machine.
3. libsql bottomless: the old lane.

Chosen: 1, behind the `object_store` crate (0.14.1, crates.io read today).

## Turso vs SQLite feature delta that matters here

CDC (a per-actor logical change log, also the validator's logical diff) and incremental
materialized views (the doc's "derived tables"). SQLite's session extension covers the first;
nothing covers the second. Everything else (MVCC, async API, vectors, FTS, encryption) is not
needed by one-writer-per-file.
