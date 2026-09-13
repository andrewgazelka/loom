# Loom World v0: the office multiverse on actors

Status: contract v0, 2026-09-12. Read `docs/architecture.md` first, then
`docs/actors-turso.md` Addendum E (capabilities), then `docs/ui-view-actor.md`
(subscriptions). The decision that put the world on Loom is
`/Volumes/Projects/wot/design/15-loom-world.md`; the browser client lives in the
`wot` repo (`client/`). This file is the wire and table contract both sides build to.

## The goal, as commands

    scripts/world-e2e.sh        # prints N/8 against a real loomd on 127.0.0.1:8797
    scripts/world-browser.sh    # prints N/3 with two headless browsers (agent-browser)

Baseline 2026-09-12: both scripts absent, 0/8 and 0/3.

The eight checks, in order (each names the observable it reads):

1. `behaviors` lists `home-v1`, `chunk-v1`, `agent-v1`.
2. Two tokens A and B each get a distinct home actor from `world.session`; calling it
   again returns the same id.
3. Presence: A's `pos` frame arrives on B's presence socket within 1 s, stamped with
   A's home id; B's socket sees a `leave` for A within 2 s of A closing.
4. Claim: A relays `claim{slot:0}` to chunk (0,0,0); B's subscription to `plots` receives a
   delta row with `owner` = A's home; B's `claim{slot:0}` produces a `rejected` row (no dead
   letter, chunk status stays `running`).
5. Build: A relays three `place` messages; B's `blocks` subscription reaches 3 rows; B's
   `place` into slot 0 produces a `rejected` row.
6. Agent: A places an `agent_screen` block; a `screens` row appears with a non-null `actor`;
   A's home receives a `hold` for that actor; A relays `prompt{text:"hello"}` to it; with the
   mock backend, B's subscription to `messages` on the agent actor reaches >= 4 rows, the
   last with `kind = "done"`; the turn's `state` is `done`.
7. Signal: A sends `{signal:{to:B, payload:{x:1}}}`; B's presence socket receives
   `{signal:{from:A, payload:{x:1}}}`; a signal to an unknown home returns an `error` frame
   to A only.
8. Restart: loomd is killed and restarted on the same data dir; `plots` has 1 row, `blocks`
   3 rows, the agent's `messages` count is unchanged (read through `actor_sql`).

Browser checks (`scripts/world-browser.sh`, two `agent-browser --session` tabs):
1. both tabs report a renderer backend (`webgpu` or `webgl2`) in `window.__wot.backend`;
2. A's avatar mesh exists in B (`window.__wot.remotes` has A's home id) within 2 s;
3. a block placed in A appears in B (`window.__wot.blocks(slot)` count) within 2 s.

## Pieces

    browser (wot/client)
      | POST /v1/command   world.session, world.chunk, world.relay, world.homes (+ existing actor_* verbs)
      | GET  /v1/stream    table subscriptions (snapshot, then deltas)   [lands with branch r5-land]
      | GET  /v1/presence  positions, roster, WebRTC signalling (in memory, never stored)
      v
    loomd
      crate loom-world:  home-v1, chunk-v1            (lane w-world)
      crate loom-agent:  agent-v1 + agent driver        (lane w-agent)
      loom-api:          world.* verbs, /v1/presence    (lane w-world)

Everything durable is an actor table. Everything at 20 Hz is the presence relay.

## Identity: one token, one home actor

- A bearer token (the `Authorizer` in `crates/loom-api/src/auth.rs`; many tokens via
  `--tokens-file`, each `{token, scopes}`) is one person.
- `world.session` resolves the caller's home actor: directory name
  `home:<blake3(token) as hex>`; `whereis` first, else `spawn_root("home-v1", init)` and
  `register`. The token never enters any actor file or log; only its hash does.
- THE RULE: a browser never sends to an actor directly. It relays through its home
  (`world.relay`), so the receiving actor's `inbox.sender` is the home id, stamped by the
  pump (`crates/loom-actor/src/pump.rs::deliver_message`), never by the payload. Every
  application permission below is a comparison against that sender.
- Capabilities: the world verbs mint with `Node::cap_for(id, rights)`
  (`crates/loom-actor/src/capability.rs:195`) and deliver the token to the caller's home as a
  `hold` message. The home stores it (`cx.accept`) and later uses it (`cx.cap(cap_id)` +
  `cx.send`). The browser learns its held caps by subscribing to its home's `held` table.

## Verbs (loom-proto `verbs.rs`, new `Family::World`; scope Execute unless noted)

| verb | args | result |
|---|---|---|
| `world.session` | `{name?: string}` | `{home: id, name: string}`; sets the name when given |
| `world.chunk` | `{universe: string, cx: int, cy: int}` | `{actor: id, cap_id: int, cap: string}`: resolves or spawns the chunk (directory name `chunk:<universe>:<cx>:<cy>`, init `{universe,cx,cy,seed}` with `seed = blake3(universe)` first 8 bytes as u64), mints SEND+INSPECT for it, delivers `hold{kind:"chunk", label:"<universe>:<cx>:<cy>"}` to the caller's home with key `hold:<home>:<chunk>` (idempotent), returns the cap token JSON string for `/v1/stream` |
| `world.relay` | `{cap_id: int, msg: json, key?: string}` | `{cursor: int}`: `node.send(home, key, {"type":"relay","cap_id":cap_id,"msg":msg})`; default key `relay:<home>:<ulid>`; the home forwards through the held cap |
| `world.homes` | `{}` (Read) | `[{home, name}]` for name tags |

`world.*` verbs are `Family::World`; `command_scope` maps them like other verbs. They are
dispatched in `crates/loom-api/src/commands.rs` next to the actor family. Unknown token:
401 as today.

## home-v1 (crate loom-world)

Schema:

    CREATE TABLE IF NOT EXISTS profile(key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS held(cap_id INTEGER PRIMARY KEY, kind TEXT NOT NULL, label TEXT NOT NULL, target TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS rejected(seq INTEGER PRIMARY KEY, sender TEXT, reason TEXT NOT NULL, msg TEXT NOT NULL);

Init message: `{"type":"init","token_hash":"<hex>","name":"<display name>"}`.

Messages (JSON; `sender` is `cx.sender()`; the operator's `node.send` arrives as `None`):

- `hold {kind, label, cap}`: sender None or self only. `cx.accept(cap)`, upsert `held`.
- `set_name {name}`: sender None only. Upsert `profile('name')`.
- `relay {cap_id, msg}`: sender None only (only the ingress injects relays). `cx.cap(cap_id)`
  then `cx.send(&cap, msg as bytes)`. A cap that is not held is a `rejected` row, not a trap.
- `delta`, `snapshot`, `resnapshot` frames from subscriptions the home may hold: ignored.
- Anything else: a `rejected` row.

A `rejected` row is the refusal channel for the whole world: `seq` = the inbox seq, `sender`,
`reason` (short, stable strings such as `not_owner`, `slot_taken`, `unknown_cap`,
`bad_message`), `msg` = the JSON text. Handlers return `Ok(())` after writing one. `Trap` is
reserved for bugs (malformed runtime state), never for a stranger's message.

## chunk-v1 (crate loom-world)

One actor per 4x4 cells (a chunk, 96 m). `slot` 0..15 indexes the cell (`slot = ly*4 + lx`).
Init: `{"type":"init","universe":"...","cx":0,"cy":0,"seed":<u64>}`.

Schema:

    CREATE TABLE IF NOT EXISTS plots(slot INTEGER PRIMARY KEY, owner TEXT NOT NULL, name TEXT NOT NULL DEFAULT '', claimed_at INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS blocks(id INTEGER PRIMARY KEY AUTOINCREMENT, slot INTEGER NOT NULL, x INTEGER NOT NULL, y INTEGER NOT NULL, z INTEGER NOT NULL, kind TEXT NOT NULL, placed_by TEXT NOT NULL, UNIQUE(slot, x, y, z));
    CREATE TABLE IF NOT EXISTS screens(id INTEGER PRIMARY KEY AUTOINCREMENT, slot INTEGER NOT NULL, kind TEXT NOT NULL, x INTEGER NOT NULL, y INTEGER NOT NULL, z INTEGER NOT NULL, yaw INTEGER NOT NULL DEFAULT 0, owner TEXT NOT NULL, actor TEXT, actor_cap INTEGER, control TEXT, UNIQUE(slot, x, y, z));
    CREATE TABLE IF NOT EXISTS homes(home TEXT PRIMARY KEY, cap_id INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS rejected(seq INTEGER PRIMARY KEY, sender TEXT, reason TEXT NOT NULL, msg TEXT NOT NULL);

`homes` is how the chunk talks back to people: `world.chunk` mints `node.cap_for(home, SEND)`
and injects `visitor {home, cap}` into the chunk (key `visitor:<chunk>:<home>`, idempotent); the
chunk `cx.accept`s it and records `(home, cap_id)`. Every `hold` the chunk sends to a home goes
through `cx.cap(cap_id)` from this table.

Block kinds (allowlist): `wall`, `floor`, `desk`, `chair`, `table`, `shelf`, `plant`, `lamp`,
`agent_screen`, `share_screen`. Coordinates: `0 <= x, z < 24` (metres inside the cell),
`0 <= y < 8`; `yaw` in `{0, 90, 180, 270}`.

Messages (sender S = a home id; sender None is `rejected(reason="no_sender")` except `visitor`):

- `visitor {home, cap}`: sender None only. `cx.accept(cap)`; upsert `homes`.
- `claim {slot}`: if `plots[slot].owner` exists and != S: `rejected(slot_taken)`. Else insert
  or keep, `claimed_at = cx.now()`.
- `release {slot}`: owner only (`not_owner`). Deletes the plot, its blocks and its screens;
  each screen with an `actor` gets `cx.stop(cap, "released")` using the child cap from the
  parent's `caps` table (`cx.cap`).
- `place {slot, x, y, z, kind, yaw?}`: owner only; kind in the allowlist (`bad_kind`); bounds
  (`out_of_bounds`). Insert or replace the block. For `agent_screen`: `cx.spawn` a child
  `ChildSpec::new("agent-v1", init, ChildType::Worker)` with init
  `{"type":"init","owner":S,"place":"<universe>/<cx>/<cy>/<slot>"}`, insert a `screens` row
  with `actor` = the child id, and send S's home
  `hold{kind:"screen", label:"<universe>:<cx>:<cy>:<slot>:<x>,<y>,<z>", cap: attenuate(child cap, SEND|INSPECT)}`
  through the SEND cap the chunk holds for that home (`homes` table, below); the child cap
  id returned by `cx.spawn` is stored in `screens.actor_cap`. For `share_screen`: a
  `screens` row with `actor` NULL, `actor_cap` NULL and `control` NULL.
- `remove {slot, x, y, z}`: owner only. Deletes the block; if a screen sits there, deletes it
  and stops its actor.
- `share {screen_id, to}`: owner only; the target `to` must be a known home (row in `homes`);
  sends `to`'s home `hold{kind:"screen", ..., cap: attenuate(child cap, SEND|INSPECT)}`.
- `watch {screen_id}`: any sender; sends S's home `hold{kind:"screen", ..., cap: attenuate(child cap, INSPECT)}`.
- `control {screen_id}` / `uncontrol {screen_id}`: for `share_screen` rows; sets `control` to S
  (or NULL when S holds it). `control` names who is sharing. Owner or current holder only.

Every refusal is one `rejected` row; the chunk never traps on input.

## agent-v1 (crate loom-agent): a Claude Code session that is not a TUI

Init: `{"type":"init","owner":<home id>,"place":"<universe>/<cx>/<cy>/<slot>"}`.

Schema:

    CREATE TABLE IF NOT EXISTS turns(id INTEGER PRIMARY KEY AUTOINCREMENT, prompt TEXT NOT NULL, state TEXT NOT NULL, started_at INTEGER NOT NULL, ended_at INTEGER, driver TEXT);
    CREATE TABLE IF NOT EXISTS messages(id INTEGER PRIMARY KEY AUTOINCREMENT, turn INTEGER NOT NULL, role TEXT NOT NULL, kind TEXT NOT NULL, body TEXT NOT NULL, at INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS rejected(seq INTEGER PRIMARY KEY, sender TEXT, reason TEXT NOT NULL, msg TEXT NOT NULL);

`role` in `user | assistant | tool | system`; `kind` in `text | tool_use | tool_result |
status | done | error`. `body` is text for `text/status/error`, JSON text for
`tool_use` (`{name, input}`) and `tool_result` (`{name, output}`), and for `done` the JSON
`{"status":"ok"|"error","exit":int}`.

Messages:

- `prompt {text}`: sender must equal `owner` (`not_owner`); a running turn exists
  (`turns.state='running'`): `rejected(busy)`. Else insert the turn (`running`,
  `started_at = cx.now()`), insert `messages(user, text)`, and
  `cx.spawn_driver(loom_agent::driver::HASH, init)` where init is
  `{"turn":id,"prompt":text,"place":"<place>","history":[{role,kind,body}...last 40]}`;
  store the driver cap id in `turns.driver`.
- `agent_event {turn, n, event}` (from the driver; sender starts with `drv:`; anything else
  is `rejected(not_driver)`): insert one `messages` row from `event`
  (`{role, kind, body}`); keyed by the driver as `agent:<turn>:<n>`, so redelivery is a no-op.
- `agent_done {turn, status, exit}`: insert `messages(system, done, ...)`, set
  `turns.state = status`, `ended_at`.
- `cancel {}`: owner only; if a turn is running, stop the driver (`cx.stop(driver cap,
  "cancel")`) and mark the turn `cancelled`.

The driver (`crate loom-agent`, `impl loom_actor::Driver`, `HASH = "agent-driver-v1"`) owns
one subprocess or one mock generator per turn and injects events through
`DriverContext::inject(owner cap, key, bytes)`:

- backend by env `LOOM_AGENT_BACKEND`: `mock` (default) or `claude`.
- `mock`: emits, 30 ms apart: `status "thinking"`, three `text` chunks that together read
  `Mock agent here. You said: <prompt>`, one `tool_use {"name":"write_file","input":{"path":"README.md","content":"..."}}`
  that actually writes `README.md` in the place directory, one `tool_result {"name":"write_file","output":"ok"}`,
  then `agent_done {status:"ok", exit:0}`. Deterministic given the prompt.
- `claude`: runs `claude -p <prompt> --output-format stream-json --verbose --permission-mode acceptEdits`
  (extra args from `LOOM_AGENT_CLAUDE_ARGS`, split on spaces) with cwd = the place directory,
  parses one JSON object per stdout line, and maps: assistant `text` content blocks to
  `assistant/text`, `tool_use` blocks to `assistant/tool_use`, user `tool_result` blocks to
  `tool/tool_result`, the final `result` line to `agent_done`; stderr lines become
  `system/status`. Exit non-zero: `agent_done {status:"error", exit}`.
- The place directory is `<places_dir>/<place>` where `places_dir` is loomd's new
  `--places-dir` (default `<actors_dir>/../places`); created at agent spawn (`init`), never
  deleted by the runtime.

## Presence relay: `GET /v1/presence` (loom-api; in memory only)

Handshake: first text frame `{"token":"...","home":"<home id>"}`; the server checks the token
(Read scope) and that `whereis("home:<blake3(token)>")` equals `home`; replies
`{"ok":true,"home":"..."}` or closes.

Client to server:

- `{"join":["<universe>:<cx>:<cy>", ...]}`: replaces the socket's room set (max 9 rooms).
  The server answers with one `{"roster":{"room":"...","homes":[...]}}` per joined room and
  broadcasts the updated roster to the room.
- `{"pos":{"cx":int,"cy":int,"x":float,"y":float,"z":float,"yaw":float}}`: stamped
  `{"pos":{"home","cx","cy","x","y","z","yaw","t":<server ms>}}` and fanned out to every other
  socket sharing at least one room with the sender. More than 30 per second from one socket:
  dropped silently.
- `{"signal":{"to":"<home id>","payload":<any>}}`: delivered to every socket of that home as
  `{"signal":{"from":"<home id>","payload":...}}`; unknown home: `{"error":"unknown_home"}`
  to the sender only. Payload limit 64 KiB.

Server to client, additionally: `{"leave":{"home":"..."}}` to each room a closed socket was
in (only when that home has no other socket in the room).

No actor is involved; nothing is stored; a restart forgets everything.

## Subscriptions (`GET /v1/stream`, exact frames from branch r5-land)

Handshake: `{"token":"..."}` then `{"ok":true}`. Subscribe with
`{"subscribe":{"actor":"<id>","table":"<t>","cap":"<Cap as JSON string>"}}` (the string the
world verbs return; INSPECT required). The socket then receives one JSON text frame per
event, each an envelope `{"key":"...","frame":{...}}` with `frame.type`:

- `snapshot`: `{type, source, seq, key:"control", table, change_id, rows:[<row object>...]}`.
  Row objects are keyed by column name.
- `delta`: `{type, source, seq, key:<inbox key of the producing message>, cause, rows:[
  {change_id, change_type, table, id, before, after, updates}]}`; `change_type` is
  `1` insert, `0` update, `-1` delete (turso CDC); `id` is the rowid; `before`/`after` are
  row objects or null.
- `resnapshot`: `{type, source, seq, key:"control", table}` followed by a fresh `snapshot`.

The browser keeps one replica per (actor, table): `Map<rowid, row>`; snapshot replaces it,
delta applies rows in order. Optimistic rows clear when a delta arrives whose `key` equals
the relay key the client chose (`world.relay` `key`).

## Client module contract (wot/client)

See `/Volumes/Projects/wot/design/16-client-contract.md`.

## Code map

| file | change | lane |
|---|---|---|
| `crates/loom-world/` (new) | home-v1, chunk-v1, tests | w-world |
| `crates/loom-agent/` (new) | agent-v1, driver (mock, claude), tests | w-agent |
| `crates/loom-proto/src/verbs.rs` | `Family::World`, four verbs | w-world |
| `crates/loom-api/src/world.rs` (new), `commands.rs`, `auth.rs`, `http.rs` | verb dispatch, session hash, `/v1/presence` | w-world |
| `crates/loomd/src/main.rs` | register `loom_world::behaviors()` and `loom_agent::behaviors()`, `--places-dir` | w-world (+ w-agent adds its line) |
| `scripts/world-e2e.sh`, `scripts/world-e2e.ts`, `scripts/world-browser.sh` | the goal commands | w-e2e |

## Not in v0

Placement across nodes, LiveKit, cloud browsers, per-tenant clusters, agent tools that edit
the world (the agent edits its place directory only), promote of agent behaviors from the
UI, voice.
