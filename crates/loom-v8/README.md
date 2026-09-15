# Loom V8 sandbox

`loom-v8` implements the shared `loom_sandbox::Sandbox` interface for JavaScript. Create a `V8Engine` with `Limits` and compile a source string. For ordinary functions, `call(positional_json, effects)` takes a JSON array of positional arguments and a borrowed `CallEffects` handler. For actors, `call_message(raw_bytes, effects)` passes the payload directly without expanding it into a JSON array of decimal bytes.

The Loom CLI defaults to TypeScript: `loom add actor.ts --name actor`. Admission uses pinned `deno_ast` for source-only TypeScript and the module bundling path below for imports or exports. Neither path performs semantic type checking. `--lang javascript` and `--lang rust` select those languages explicitly. TypeScript uses the same Loom guest APIs as JavaScript and has no Deno globals or operating-system access.

```javascript
const LOOM_SCHEMA = 'CREATE TABLE counter(value INTEGER);';

async function main(message) {
    return await loom.perform('example', {message});
}
```

`main` must be a function. Optional `LOOM_SCHEMA` must be SQL text. `loom.perform(op, args)` forwards a JSON effect descriptor to the caller, which supplies Loom's durable actor operations and capability checks. The sandbox does not grant actor access by itself. Host errors abort the invocation and retain their Rust error type, even when JavaScript catches rejected promises.

`loom.now()`, `loom.random(n)`, and `loom.sql(sql, params)` use the same effect bridge. Time and randomness therefore follow the actor's recorded effects; ambient `Date`, `Intl`, and `Math.random` are unavailable. The `loom` binding and its methods are frozen.

`loom.sql` accepts JavaScript strings, numbers, and `null` as parameters and returns row objects keyed by column name. Use `loom.sql.blob(bytes)` for a binary parameter. The helper translates values to the host's tagged SQL representation.

`loom.actors.get(cap)` wraps capability bytes in a frozen actor handle. `await loom.actors.accept(cap)` verifies and records the capability before returning its handle. `loom.actors.spawn(spec)` and `loom.actors.self()` also return handles. Handles provide `send(value)`, `sendBytes(bytes)`, `call(value, {timeoutMs})`, `reply(reference, value)`, `sendAfter(ms, value)`, and `stop(reason)`. Calls return a reference; replies arrive as later messages. The host applies the same authorization checks as for `loom.perform`.

`await loom.actors.named(name)` resolves a host-published actor within the current tenant. `await loom.actors.sender()` returns the current message's sender handle. A handle serializes as its capability bytes, so actors can pass handles inside JSON messages. The recipient calls `accept` to record a handed-off capability; `get` only reconstructs a handle for a capability it already holds. Actor IDs alone grant no authority.

`loom.actors.spawn({behavior: behaviorHash, init: {count: 0}})` creates a child with JSON initialization. The optional fields are `durability`, `restart`, `shutdown`, `link`, `monitor`, and `type`; see [api.d.ts](api.d.ts) for their types and all guest declarations.

The daemon's `--tokens-file` contains entries such as `{"token":"secret","tenant":"acme","scopes":["read","execute","define"]}` in a JSON array. Authentication selects the tenant. Guest actor, process, and WebSocket helpers accept no tenant override.

`loom.messages.encode(value)` and `loom.messages.decode(bytes)` convert between JavaScript values and Loom message bytes. `loom.messages.json(handler)` wraps a value handler for an actor's byte-message entry point. Functions without a return value complete with `null`. Host integers outside JavaScript's exact integer range are rejected before entering the isolate.

Every invocation creates a fresh isolate. JavaScript globals and prototype changes do not persist between calls; durable state belongs in actor storage. The engine reuses compiled-code cache bytes and bounds worker concurrency and queued work. `Promise.all` can submit multiple effects, which the borrowed host handler executes serially.

A host effect can call another sandbox on the same engine, even with one worker. While waiting for host replies, workers execute queued calls in nested isolates. `max_reentrant_depth` bounds that nesting; exceeding it fails the invocation instead of waiting for a worker that cannot become free.

## Module admission

TypeScript and JavaScript support static imports from `npm:`, `jsr:`, and approved HTTPS origins. Modules may define a local `main` function or constant, or use a named `export function main`. A default-only export does not provide the required `main` binding. Dynamic imports are rejected.

Admission uses pinned Deno 2.9.6 and esbuild 0.25.5 to produce a browser-targeted bundle without running package lifecycle scripts. Loom's content-addressed store retains emitted JavaScript, the dependency lock, a source map containing dependency sources, compiler identity, and origin policy. Runtime reopen reads the stored artifact offline; actor execution has no live module loader.

The default approved HTTPS origins are `deno.land`, `jsr.io`, `registry.npmjs.org`, `esm.sh`, `raw.esm.sh`, and `cdn.jsdelivr.net`, on port 443. The host can add origins with `Compiler::allow_import`. Imported code has the same Loom capabilities and runtime restrictions as the importing actor. Packages that require Node, Deno, filesystem, or ambient network APIs cannot use those APIs here; use Loom's actor, process, and WebSocket handles for host operations.

## Actor lifecycle

`loom.actor({onStart, onMessage, onStop})` creates a JSON message handler with optional host lifecycle hooks. `onMessage` is required. The host invokes `onStart` after schema setup and before ordinary initialization or inbox messages, once per activation. Fresh isolates for subsequent messages do not invoke it again. Graceful `Node.close()` invokes `onStop("node_shutdown")`; an abrupt host exit cannot run that hook. Reopening the node activates the actor and invokes `onStart` again.

Only the host invokes lifecycle hooks. An incoming message with `type: "onStart"` or `type: "onStop"` remains an ordinary message. Existing `loom.messages.json` handlers remain valid when no lifecycle hooks are needed.

Persist desired container configuration in SQL to resume a workflow after node restart. `onStart` can read that configuration, create a fresh container, and replace the saved capability. This resumes the actor's durable workflow; it does not restore container process memory. Since `onStart` precedes initialization messages, seed required defaults in `LOOM_SCHEMA`, or have the first configuration message save its configuration and start the container itself.

The standalone [container actor](../../examples/container-actor/main.ts) stores its image choice in SQL, creates a fresh container in `onStart`, records process output in `onMessage`, and cancels its saved capability in `onStop`. The [daemon smoke test](../../tools/smoke-loomd-v8.py) loads that same source and checks container identity and output after restart. See the [run instructions](../../README.md#native-container-and-lifecycle-smoke).

## Processes

The host publishes fixed process presets through `--processes-file`, a JSON array of entries with `tenant`, `name`, and `spec` fields. `spec` is the host's `ProcessSpec`; guest code cannot supply command paths, arguments, or environment. `await loom.processes.named("claude")` resolves the existing registered process actor. `await loom.processes.spawn("claude", options)` creates a new instance of the same preset.

Each preset entry also requires a `sandbox` policy with `readonly` absolute runtime paths and a `network` boolean. Include the executable and its runtime closure in those paths. Preset paths cannot overlap Loom's protected state or source tree; the sandbox policy participates in the preset's identity.

Subscribe during spawn to receive its output from the start. Store the handle in SQL if later messages need it:

```javascript
const LOOM_SCHEMA = "CREATE TABLE processes(name TEXT PRIMARY KEY, cap TEXT NOT NULL)";

const main = loom.messages.json(async message => {
    if (message === null) {
        const process = await loom.processes.spawn("claude", {
            subscriber: await loom.actors.self(),
        });
        await loom.sql("INSERT INTO processes VALUES (?, ?)", ["claude", JSON.stringify(process)]);
    } else if (message.type === "input") {
        const [row] = await loom.sql("SELECT cap FROM processes WHERE name = ?", ["claude"]);
        await loom.processes.get(JSON.parse(row.cap)).write(message.text);
    }
});
```

Process output arrives as `{type: "process.output", stream: "stdout" | "stderr", bytes: [...]}`. Handles also provide `closeStdin()`, `cancel()`, and `subscribe(actor)`. Later subscriptions do not promise replay of earlier output. Processes do not automatically restart after exit or host restart; a stored capability does not restart a process.

## Temporary containers

The host enables containers with `--docker-executable /absolute/path/to/docker`. Optional `--docker-host ENDPOINT` selects its Docker endpoint and requires `--docker-executable`. The daemon registers the container actor for each tenant.

`loom.containers.spawn(spec)` runs an image through the tenant's host-configured Docker connection and returns the same process handle as `loom.processes.spawn`. Container output uses the `process.*` messages, including `process.output`. Subscribe at spawn to receive output from the start:

```javascript
const sandbox = await loom.containers.spawn({
    image: "alpine:3.22",
    command: "cat",
    network: "none",
    env: {LANG: "C"},
    limits: {memoryMb: 128, cpus: 1, pids: 32},
    ttlMs: 60000,
    subscriber: await loom.actors.self(),
});
await sandbox.write("hello\n");
await sandbox.closeStdin();
```

`image` is required. Optional `command`, `args`, and `env` select the program inside the container. `limits` accepts `memoryMb`, `cpus`, and `pids`; omitted values default to 512 MiB, one CPU, and 128 processes. `ttlMs` defaults to one hour. The returned handle provides `write`, `closeStdin`, `cancel`, and `subscribe`. Store its capability in SQL and restore it with `loom.processes.get(cap)` between actor messages.

`network` accepts `"none"` for offline execution or `"bridge"` for external API access, such as Claude. It defaults to `"bridge"`. Guests cannot select host or custom networks, configure the Docker socket, or supply mounts or privileges. The host's Docker connection determines where containers run.

The container receives only its explicit `env` map, without inherited host environment variables. Loom removes containers on exit, cancellation, actor stop, or TTL expiry. The TTL watchdog runs in `loomd`; a container can continue running while the daemon is down. On startup, the daemon reconciles old containers by tenant and a durable random owner ID stored in that tenant's database.

## WebSockets

`loom.websockets.listen({maxConnections: 64, maxMessageBytes: 1048576})` starts a native listener driver and returns its actor handle. The optional limits permit 1–4096 connections and 1–16777216 bytes per message. The driver handle controls the listener; each connected socket has a separate capability. Use `loom.websockets.sender()` in a socket event handler to get that connection's handle.

This dedicated listener actor echoes text and binary messages and listens again when its driver goes down:

```javascript
const main = loom.messages.json(async message => {
    if (message === null || message.type === "down") {
        await loom.websockets.listen();
    } else if (message.type === "websocket.message") {
        const socket = await loom.websockets.sender();
        if (message.data.type === "text") await socket.send(message.data.text);
        else await socket.sendBytes(message.data.bytes);
    }
});
```

Register this actor with `loom register chat ACTOR_ID`. The browser connects to the same-origin `/v1/actors/chat/websocket` route with its tenant token. Browsers cannot set an `Authorization` header on `WebSocket`, so supply the token as a base64url-encoded UTF-8 subprotocol:

```javascript
function connectActor(name, token) {
    const url = new URL(`/v1/actors/${encodeURIComponent(name)}/websocket`, location.href);
    url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
    const bytes = new TextEncoder().encode(token);
    const encoded = btoa(Array.from(bytes, byte => String.fromCharCode(byte)).join(""))
        .replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
    return new WebSocket(url, ["loom.actor.v1", `loom.auth.${encoded}`]);
}
```

The server negotiates only `loom.actor.v1`. Native clients may use an `Authorization: Bearer TOKEN` header instead; sending both authentication forms returns HTTP 400.

The listener emits `websocket.listening`, then socket events `websocket.open` with `connection` and `protocol`, `websocket.message` with `connection` and `data`, and `websocket.close` with `connection`, `code`, `reason`, and `clean`. Message data is `{type: "text", text}` or `{type: "binary", bytes}`. Socket handles provide `send(text)`, `sendBytes(bytes)`, and `close(code, reason)`; reconstruct a persisted handle with `loom.websockets.get(cap)`.

Connections live in the native host and survive fresh isolates between actor messages. Host restart closes connections and reports the listener driver's `down` event. The actor must listen again, and browsers must reconnect to obtain new connections. Stored socket capabilities stop working when their sockets close.

## Limits

`Limits` controls the execution deadline, JavaScript heap, source and JSON message sizes, pending effects, worker count, and queue capacity. Cancellation terminates guest execution and closes its host bridge. An unresolved promise with no outstanding host effect fails.

The heap limit does not bound process RSS, V8 code, or all native allocations. Use process or container resource limits when a total memory bound is required. This build enables V8 pointer compression. V8's memory-corruption sandbox cage is **not enabled** because the required official binary archive was unavailable. Isolates restrict JavaScript host APIs; they do not provide a process security boundary against a V8 memory-corruption exploit.

The bootstrap hides ArrayBuffer, typed-array, WebAssembly, and shared-memory constructors. It also removes `Temporal`, `WeakRef`, and `FinalizationRegistry` to keep ambient time and garbage-collection observations out of guest execution. The guest interface carries JSON values only. No network, filesystem, Node.js, or Deno APIs are installed.

Run the native integration tests with `cargo test -p loom-v8`. They exercise V8 execution, borrowed effects, typed host errors, cancellation, deadlines, fresh state, and message limits.
