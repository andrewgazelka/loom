# Actor model decisions (summary; the spec is docs/actors-turso.md)

- An actor = one Turso file + one mailbox + one behavior addressed by content hash.
- The one invariant: one message = one transaction containing the inbox cursor, domain
  writes, effect rows, outbox rows. Nothing external inside except keyed short effects.
- Sends are deferred: outbox rows delivered after commit by a pump; per-pair FIFO; a stopping
  actor's outbox drains before `exit`/`down` fan-out; keyed inserts make redelivery exactly-once
  in effect. This is Durable Objects' output gate as a table.
- OTP parity, decided primitive by primitive (Addendum B): links, monitors with flush, stop
  reasons, untrappable kill, trap_exit with `normal` ignored, timers with cancel, call/reply
  with monitor + timeout, defer as selective receive (livelock is a trap), terminate and
  data migration hooks, promote_where, supervisor strategies one_for_one/one_for_all/rest_for_one/
  dynamic, intensity, child specs with shutdown timeouts, root supervisor, names and groups.
- Deliberate differences: no blocking receive mid-function (CPS via the next message), no
  scheduler priorities, no ETS-style shared mutable tables (an ETS table is an actor).
- Durable restart has three verbs: resume (retry the message), skip (dead-letter it), reset
  (archive the file, fresh state, same id and links). Without reset, "let it crash" re-hits the
  same poison forever.
- Evolution: `code_changes` rows (behavior_hash, later wasm_hash and toolchain_hash); validate
  on a fork with verdicts Matched/DivergedAt/Differs/Trapped; validation memo keyed by content;
  outbox-diff cutoff says whether receivers are affected.
- Unison mapping: identity = definition hash; validity = the effects table as a read set;
  cutoff = per-table hashes and outbox hash.
