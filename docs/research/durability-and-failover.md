# Durability and failover without Raft

## Two different "always up"

| Want | Needs | Mechanism |
|---|---|---|
| never lose state | ship WAL segments + snapshots to an object store; restore = snapshot + tail | object_store crate; local dir or S3/MinIO |
| keep serving when a machine dies | a lease per actor with an epoch that fences a stale owner | conditional writes on the object store |

## Prior art (read today)

- Cloudflare Durable Objects: one SQLite per object, output gates (no outgoing message before
  the write is durable), change log shipped to followers (blog "Zero-latency SQLite storage in
  every Durable Object"; workerd `actor-sqlite.h`).
- LiteFS: lease via a Consul key with TTL, single primary, WAL pages copied to LTX files
  (fly.io/docs/litefs/how-it-works, ARCHITECTURE.md).
- Litestream v0.5: LTX, point-in-time restore, VFS read replica from S3.
- Restate, Temporal: journaled non-deterministic operations, replay after failure.

## Leases by conditional PUT (recalled: S3 `If-None-Match` Aug 2024, `If-Match` on PutObject Nov 2024; MinIO supports both; verify against object_store 0.14.1 `PutMode::{Create, Update}` before relying on it)

One object per actor `{owner, epoch, expires}`: `Create` for first claim, `Update(version)`
for renew/takeover with a higher epoch after expiry. The segment `head` pointer is also a
conditional PUT, so a stale owner's upload is rejected. Lease traffic is per takeover and
renewal (seconds), never per message.

## Per-actor durability

- `local` (default): commit is a local WAL append; segments ship every ~1 s; machine loss loses
  at most that tail.
- `remote`: commit returns after the object store acks (tens of ms on a LAN against MinIO).
  Set on the actors that matter.

## Stale owner

A node whose renewal fails stops the actor with reason `lease_lost`; a rejected upload marks
the actor `lease_lost` and renames the local file `<id>.stale.<epoch>.db` so bytes remain
inspectable; re-open re-acquires. Every state names its leaver.
