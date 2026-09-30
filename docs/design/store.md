# On-disk store: blobs as files above 1 MiB, SQLite for everything relational, a persistent result cache

Status: decided 2026-09-30 from a benchmark (`/Volumes/Projects/tmp/loom-store-bench`: crate, `results/*.jsonl`, `agg.py`).
Machine: this Mac (APFS), load average 40 to 130 throughout, medians of 5 interleaved rounds, all reads served from the OS page
cache, nothing re-run on a quiet machine, no Linux, RocksDB and sled not benchmarked. Trust ratios inside a round more than absolutes.

## Decision

* **Metadata and small blobs stay in SQLite** (WAL). The schema is relational (foreign keys, views over `json_each`, UDFs) and
  open costs about 1 ms at 1M entries. Values under 1 MiB stay inline in `cas.bytes`.
* **Values of 1 MiB and up are immutable files** at `objects/<h>/<hash>` (one hex character of fan-out), written to a temp name,
  fsynced, renamed, then indexed. The `cas` row records kind, codec, size and that the bytes are external.
* **The result cache persists** in its own `cache.db` (SQLite, `auto_vacuum=INCREMENTAL`). The in-memory GreedyDual index in
  `crates/loom-rt/src/result_cache.rs` stays the authority for eviction order; the file only survives restarts.
* **Not adopted:** redb (2.0x space at 64 KB, no crash-safe commit without fsync at about 200 puts/s), heed/LMDB (fine, but adds an
  OpenLDAP-licence notice for no gain over SQLite here), fjall (168 to 199 ms to open 1M entries; the second choice for the result
  cache only if it ever needs over about 100k lookups/s), sled (beta, format will change), RocksDB (GPLv2 OR Apache-2.0: only by
  electing Apache-2.0), file per object (350x more directory entries).

## Licences (text read in ~/.cargo/registry, exact releases)

rusqlite 0.32.1 MIT (bundled SQLite 3.46.0 is public domain); redb 4.3.0 MIT OR Apache-2.0; heed 0.22.1 MIT with vendored LMDB
under the OpenLDAP Public License 2.8; fjall 3.1.10 MIT OR Apache-2.0 (its tree has `self_cell` Apache-2.0 OR GPL-2.0-only and
`r-efi` with an LGPL option: elect the permissive one); rocksdb 0.25.0 (RocksDB 11.8.1: GPLv2 OR Apache-2.0); sled 0.34.7 MIT/Apache-2.0.

## Measured (today's SQLite blob-in-table versus SQLite index plus files)

| | today | index + files |
|---|---|---|
| put 128 MB | 180 MB/s | 2,366 MB/s |
| put 16 MB | 238 MB/s | 1,243 MB/s |
| read 128 MB after reopen | 30.5 ms | 8.9 ms |
| read 16 MB | 2.43 ms | 1.27 ms |
| read 1 MB | 0.13 ms | 0.10 ms |
| read 64 KB | 0.012 ms (inline) | 0.035 ms (file) |
| open, 1M entries | 0.4 to 2.3 ms | 0.4 to 1.8 ms |
| disk after 1000 puts | 1.00x | 1.00x |

Also: hashing 128 MB with BLAKE3 took 139 ms against 8.1 ms to `read()` it, and `Store::get` re-hashes on every read. A
hard link of a 46.6 MB object took 0.22 ms against 8.7 ms to read and write it; `artifact_restore_ms` was 1,701 of about 3,400 ms
in the 2026-09-21 warm baseline. On macOS Rust's `sync_all` is F_FULLFSYNC (about 4.4 to 6 ms), so fsyncs are batched, not per blob.

## Files per 100k objects (the Nix problem)

From a real 7,879-object store (192.6 MB): objects of 1 MiB and up are 0.27% of objects and 63% of bytes. Threshold 1 MiB gives
about 267 blob files in 16 directories (about 287 entries per 100k objects); threshold 64 KiB about 2,770; file per object 100,000+.

## Rules for the implementation

1. Order of a spilled write: file written to `objects/tmp/<random>`, `sync_all`, `rename` into place, then the index row. A row
   without its file cannot exist; an orphan file after a crash is harmless and swept by a scan. Directory syncs are batched at the
   store's existing durability barrier (`durability.rs`), not per blob.
2. Reading a spilled blob verifies its BLAKE3 once per process per hash (a bounded set), not on every read; a background scrub can
   verify the rest. Inline values keep verifying as today.
3. Restoring a spilled blob to a build directory uses `clonefile` on APFS, else a hard link when the consumer only reads, else a copy.
4. The result cache file is disposable: on any corruption it is deleted and starts empty. Hit counts are flushed in batches (60 to
   73k bumps/s at 1000 per transaction). Eviction deletes rows in one transaction, then unlinks spilled files.
5. Later, separately: `cas.hash` as a 32-byte BLOB instead of hex TEXT (halves index size, touches every `REFERENCES cas(hash)`);
   never `WITHOUT ROWID` on tables with inline blobs (4.6x space at 1 KB rows in the first run).
