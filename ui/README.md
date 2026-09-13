# Loom REPL UI

A static SvelteKit workspace for Rust definitions and durable actors. Definitions
and all 22 MCP actor operations use the HTTP contract in [API.md](API.md).

```sh
bun install --frozen-lockfile
bun scripts/verify.ts
bun run dev --port 5186 --strictPort
```

The verification command runs `bun run check`, `bun run build`, and `bun test tests`,
prints each exit status, then prints `N/3 UI gates pass`. Unit tests
cover every command fixture, request encoding, input and response rejection,
validation verdicts, tree identity joins, superseded request ownership, and immutable command history snapshots.

The development server proxies `/v1` to `127.0.0.1:8787`. Connection settings
accept an optional HTTP endpoint and bearer token. Manual connections and launcher fragment tokens are saved as `{ endpoint, token }`
in localStorage under `loom.connection`. Open `/#token=<URL-encoded token>` to
connect to the page origin automatically; the fragment is removed before requests.
Query-string tokens are rejected because they reach server logs. The
static build goes to `build/`; production must serve `index.html` and route
`/v1` to the daemon. HTTP errors remain visible with operation context.

Open `http://127.0.0.1:5186/?mock=1` for the static fixture preview without a daemon.
Use `&panel=actor_validate&verdict=Differs` to inspect a specific panel and verdict.
Every command in the palette has a fixture. Mock mutations return snapshots,
not a simulated database. Unknown fixtures fail explicitly. Guest Rust examples
use plain crate-root `pub fn` entries, without attributes or macros. Schemas use
`pub const LOOM_SCHEMA: &str`.

The UI fills the window, follows the OS color scheme, and bundles Inter locally.
Definition and actor icons use distinct colors; selections and status indicators
use luminance. Docked panes have borders and no shadows.

- `j` / `k` moves focus through rows in the focused pane; Enter opens the row.
- `h` / `l` moves between explorer, command, and REPL history panes.
- Cmd+K opens the searchable command palette; arrows select and Enter opens.
- Cmd+Enter runs the focused command form, including CodeMirror editors.
- Esc in an editor focuses the panel list. `r` reruns the focused history entry.
- `?` toggles key hints, hidden by default. `+` / `-` changes type scale.

Shortcuts leave text input and browser modifier shortcuts intact. Live mutations
run only on explicit form submission or a history rerun. Replacing a panel aborts its pending read
and prevents late results from replacing the newly selected identity.

Rust and JSON inputs use CodeMirror 6 with line numbers, bracket matching, and
OS-aware syntax colors. SQL inputs use the SQL grammar. Shiki renders source,
item names, JSON results, and inline diffs with GitHub light/dark themes. Hash
pills show eight characters and copy the full hash. History retains the latest 100 completed commands and every running command,
including inputs, results, errors, and timestamps. It is saved per endpoint on
this device; background workspace refreshes and transport credentials are excluded.
Export downloads the retained history as JSON. Clear removes it, and is disabled
while commands are running. After reload, unfinished commands are marked
interrupted with an unknown server outcome and are never automatically rerun.

Command drafts survive panel changes and reloads, separately for each endpoint,
operation, and definition or actor. Discard draft restores the form's baseline;
Update fetches the current stored source. Replay refuses to replace an unsaved
draft or an in-flight command. Storage failures remain visible and preserve
in-memory work. Invalid saved data is preserved instead of being overwritten.

The workspace owns in-flight commands, so navigating away does not abort an Add
or lose its result. Add and Update show elapsed time and the server's actual
preflight, checking, compilation, and publication stages from authenticated
`GET /v1/builds/active`. Compiler output is available after a successful build.
Run shows parameter names, protocol types, and a JSON example. It accepts the same
JSON values as HTTP and MCP, including single scalar and object arguments.

## Browser verification

Use Playwright through `repl.ts`, connected to the existing browser at
`http://localhost:9222`. Open a new task tab. Test live commands against the
packaged daemon in addition to fixtures: scalar Run, draft navigation/reload,
replay conflicts, history export/clear, and navigating away during compilation.

Restart a static preview after rebuilding so its entry script matches the
current build. Keep the preview build fixed while capturing screenshots.

## Binding core

`src/lib/bind/{bind,patch,stream}.ts` is plain TypeScript without Svelte or Vite imports.
`bind(container, stream, { onEvent, onError })` maintains keyed row elements and
patches only changed attributes, text, and children. `pending(key, tree, messageKey)`
marks an optimistic row; an authoritative delta clears it by `key` or `cause`,
and a dead letter restores its last authoritative tree. A resnapshot preserves
surviving nodes until the complete replacement snapshot arrives. Root and keyed
child tag changes fail by key because a different element class cannot preserve identity.
Children match by key, unkeyed elements by tag in document order, and text by
position. Ordering runs after removals, moving neighbours with `insertBefore`
around the stationary focused node or its ancestor. This applies inside rows
and between rows; focus is not restored after a blur. Removing a leading sibling
preserves the unkeyed input. Causes carry only the handled delta's key, never
its incoming cause.

`stream.ts` decodes JSON blobs only at the tree-table boundary. WebSocket capabilities
remain opaque strings so JavaScript cannot round their 64-bit fields. The `/view`
shell authenticates, spawns a view, subscribes to its `tree` table, and sends event
messages `{type: name, key: rowKey, payload}` to the source through the ordinary
`send` command. Closing the page closes its subscription and requests that its
ephemeral view stop. HTTP authentication and source SEND authority remain server checks.
Events mark their row pending using its current tree. Confirmed failed sends become
local dead-letter frames; a transport failure or pending server outcome leaves the
marker until an authoritative verdict or reconnection. The shell does not retry sends.

From the repository root:

```sh
bun test ui/tests/bind
```

The five tests use isolated `happy-dom` 20.14.3 windows with MutationObserver and
focus/selection APIs. Existing UI tests had no DOM shim. This dependency is pinned
in `package.json`; the lead must update `ui/bun.lock` and install it before the gate.
No dependency installation or test execution ran in the write-only implementation lane.
