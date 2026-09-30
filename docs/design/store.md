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
hard link of a 46.6 MB object took 0.22 ms against 8.7 ms to read and write it (links were dropped afterwards, see rule 3; APFS clones keep
most of that saving); `artifact_restore_ms` was 1,701 of about 3,400 ms
in the 2026-09-21 warm baseline. On macOS Rust's `sync_all` is F_FULLFSYNC (about 4.4 to 6 ms), so fsyncs are batched, not per blob.

## Files per 100k objects (the Nix problem)

From a real 7,879-object store (192.6 MB): objects of 1 MiB and up are 0.27% of objects and 63% of bytes. Threshold 1 MiB gives
about 267 blob files in 16 directories (about 287 entries per 100k objects); threshold 64 KiB about 2,770; file per object 100,000+.

## Rules for the implementation

1. Order of a spilled write: file created exclusively (`O_EXCL`) in `objects/tmp/`, `sync_all`, rename into place, `fsync` of the
   shard directory, then the index row. The directory sync is inside the write, one extra fsync per spilled blob, because rows
   are committed by several paths (`put`, `put_file`, intake, the recording writer) and SQLite's auto-checkpoint can make a row
   durable at any commit; so a durable row never names a file a crash can lose. One gap: when a write is deduplicated (the file
   is already there) its writer may have died between rename and directory sync, so the shard is marked dirty and synced at the
   store's durability barrier (`durability.rs`); a row committed by a direct path before that barrier is not covered for that
   file. A row without its file cannot be created by this code. An orphan file (no row, after a crash or a failed commit) is
   harmless, but there is no sweep for orphan object files yet: only stale `objects/tmp/` files are removed, at open. A
   sweeper has to cope with a writer that deduplicates against an old orphan between the scan and the delete, so it is not
   written yet.
2. Reading a spilled blob verifies its BLAKE3 once per file per process, not on every read. "Once" is keyed by a stamp of the
   file (inode, size, mtime and ctime in nanoseconds) taken from the open handle, in a bounded FIFO set; any change of the stamp
   (a write through any link, a replacement) forces a re-hash on the next read or restore. A background scrub can verify the
   rest. Inline values keep verifying as today. `put`, `put_file` and intake compare an existing file by hash, not just size,
   unless its stamp is already verified, and rewrite it on mismatch; an inline put whose existing row fails its hash replaces the
   row's bytes (a corrupt inline row used to survive `INSERT OR IGNORE` forever). A missing or corrupt object is the typed
   `ObjectError` (Missing, Corrupt); callers that can rebuild match it with `ObjectError::is_in` and treat every other error as a
   real failure, not a cache miss. `Store::has_object` (row exists and the spilled file is
   present at the recorded size) is the check for a cache hit; `size_of` answers from the index alone.
3. Restoring a spilled blob to a build directory or sandbox never hard-links: it uses `clonefile` on APFS, else a copy, into an
   exclusive temp name beside the destination, then a rename. The result is an independent 0644 file (inline objects are the
   same), so a consumer that can write to it cannot reach the store's inode. The object is verified as in rule 2 through an open handle, the clone (`fclonefileat`) or copy reads that same handle, and the
   handle's stamp is re-read afterwards; a change during the restore fails it. Adopting an object from another store directory (intake, backup) is a hashed copy, refused when the source
   does not match its hash, for the same reason.
4. The result cache file is disposable: on any corruption it is deleted and starts empty. Hit counts are flushed in batches (60 to
   73k bumps/s at 1000 per transaction). Eviction deletes rows in one transaction, then unlinks spilled files; nothing else holds
   those inodes now that restores do not link, so an unlink cannot change a file a consumer is reading.
5. Later, separately: `cas.hash` as a 32-byte BLOB instead of hex TEXT (halves index size, touches every `REFERENCES cas(hash)`);
   never `WITHOUT ROWID` on tables with inline blobs (4.6x space at 1 KB rows in the first run).
