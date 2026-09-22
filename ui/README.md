# Loom UI

A static SvelteKit app over one daemon, two pages:

- `/` is the board: the overview (definitions and actors in a narrow rail; graph, feed and
  builds in the main area) and, one click deep, a Detail for any definition, actor, build or run.
  The source is the object; everything else is a header line or a tab away.
- `/workspace/` is the command workspace: a form per verb, all 22 MCP actor operations, and
  the REPL history. The board's header links to it as "commands"; every Detail has "Open in
  workspace".

Both use the HTTP contract in [API.md](API.md) and share one connection store.

```sh
bun install --frozen-lockfile
bun scripts/verify.ts
bun run dev --port 5186 --strictPort
```

The verification command runs `bun run check`, `bun run build`, and `bun test tests`,
prints each exit status, then prints `N/3 UI gates pass`. Unit tests
cover every command fixture, request encoding, input and response rejection,
validation verdicts, tree identity joins, superseded request ownership, and immutable command history snapshots.
Run `bun test` from this directory: `bunfig.toml` preloads `tests/svelte-loader.ts`, which
compiles `.svelte` imports so the pane and tab registry tests can load descriptors that
reference components (components are imported, never mounted, in unit tests).

The development server proxies `/v1` to `127.0.0.1:8787`. Connection settings
accept an optional HTTP endpoint and bearer token. Manual connections and launcher fragment tokens are saved as `{ endpoint, token }`
in localStorage under `loom.connection`. Open `/#token=<URL-encoded token>` (or
`/workspace/#token=…`) to connect to the page origin automatically; the fragment is removed
before requests, and on the board a selection fragment (`#def=…`) given alongside the token
survives it.
Query-string tokens are rejected because they reach server logs. The
static build goes to `build/`; production must serve `index.html` and route
`/v1` to the daemon. HTTP errors remain visible with operation context.

Open `http://127.0.0.1:5186/workspace/?mock=1` for the static fixture preview without a daemon.
Use `&panel=actor_validate&verdict=Differs` to inspect a specific panel and verdict.
Every command in the palette has a fixture. Mock mutations return snapshots,
not a simulated database. Unknown fixtures fail explicitly. Guest Rust examples
use plain crate-root `pub fn` entries, without attributes or macros. Schemas use
`pub const LOOM_SCHEMA: &str`.

The UI fills the window, follows the OS color scheme, and bundles Inter locally.
Definition and actor icons use distinct colors; selections and status indicators
use luminance. Docked panes have borders and no shadows.

## Command workspace (`/workspace/`)

- `j` / `k` moves focus through rows in the focused pane; Enter opens the row.
- `h` / `l` moves between explorer, command, and REPL history panes.
- Cmd+K opens the searchable command palette; arrows select and Enter opens.
- Cmd+Enter runs the focused command form, including CodeMirror editors.
- Esc in an editor focuses the panel list. `r` reruns the focused history entry.
- `?` toggles key hints, hidden by default. `+` / `-` changes type scale.

Shortcuts leave text input and browser modifier shortcuts intact. Live mutations
run only on explicit form submission or a history rerun. Replacing a panel aborts its pending read
and prevents late results from replacing the newly selected identity.

The `view` panel shows the source first: the rustfmt rendering when the server returned
`formatted_source`, the stored bytes with the `format_error` as a one-line note when it did not,
and an "as submitted" toggle for the exact stored bytes. Identities, entry effects and items sit
behind one disclosure; the query form sits behind "edit query" once a result is on screen.

The REPL history pane starts collapsed to one line (`12 commands · show`). Expanded, each row
is the verb, the time and one human line (`double(21) → 42`, `counter · a41e908f`,
`3 definitions`); the parameter dump (`target=… · actor=…`, blank parameters omitted) and the
raw input and result JSON sit behind the row's "details" toggle.

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

## Board (`/`)

One model, fed by the journal and the actor tree poll, drawn by panes from a registry. Open
`/#token=<URL-encoded token>`; the connection store is shared with the workspace
(`src/lib/workbench/connection.ts`), so a saved connection works without a fragment.

### Selection and the URL

What is in focus lives in the fragment once the token is consumed:

| Fragment | Detail |
| --- | --- |
| (none) | Overview |
| `#def=<64-hex definition hash>` | Definition |
| `#actor=<actor id>` | Actor |
| `#build=<64-hex component hash>` | Build |
| `#run=<trace scope>` | Run |

`src/lib/board/selection.ts` parses and formats it; two selection keys at once, an empty value
or a malformed hash are errors shown in the red strip, never a silent Overview. Loading such a
URL opens the Detail directly; selecting pushes a history entry, so the browser's back returns.
Everything that names a definition, actor, build or run is clickable: definition rows, graph
nodes, feed rows, build rows, actor rows, dependency links, the definition on an actor or run.
Escape or the back control (`data-testid="detail-back"`) returns to the Overview.

### Details

- Definition (`data-testid="board-detail"`, `data-hash`): one header line with name, language,
  8-char hash pill, effect badges, dependencies as links (`data-testid="detail-dep"`,
  `data-hash`), dependents count, Run (an inline JSON-array arguments box; Enter runs through the
  `run` verb and shows `output` inline) and Open in workspace. Then the tabs, source first:
  - Source (`data-testid="detail-source"`, lines `[data-line=N]`): line numbers, Shiki. Shows
    `formatted_source`; when null, the stored source and a muted note with `format_error`; the
    "as submitted" toggle is the only place the exact stored bytes appear.
  - Wasm (`data-testid="detail-wasm"`): source left, wat right (`[data-wat-line=N]`, and when
    mapped `data-src-line`, `data-src-file`). Hovering or clicking a source line marks every wat
    line compiled from it `hot` and scrolls the first into view; clicking a wat line marks its
    source line. Functions collapse: only those whose name carries an export name or the guest
    crate prefix `loom_definition` start open, the rest (std, alloc, core, SDK, ABI shims) are
    one-line headers with the name and line count. `debug: false` shows "This artifact carries no
    debug info; rebuild to map source lines." and still the text. The module is fetched once per
    component hash (`BoardClient.wasm`).
  - Items: item name and hash. History: the name's hash chain from `history`, newest first;
    expanding a revision runs `diff` against the previous hash once.
- Actor (`data-testid="detail-actor"`, `data-id`): status, cursor, definition link, parent; tabs
  Tables (`sql`: `SELECT name FROM sqlite_master WHERE type='table' ORDER BY name`, a `COUNT(*)`
  per table, and the last 50 rows by `rowid DESC` of the table you click) and Lineage.
- Build (`data-testid="detail-build"`, `data-hash`): name, hash, ms, size, rustc count, time; tabs
  Stages (the bars, large) and Log (the CAS log, fetched once, monospace).
- Run (`data-testid="detail-run"`): outcome, elapsed, entry, definition link, args hash, trace
  hash. The run is looked up in the held feed (newest 500 rows); older scopes say so.

### Panes and the panes menu

The header's "panes" control (our own popover, `data-testid="panes-menu"`, rows
`data-pane-toggle=<id>`) hides or shows Overview panes. The arrangement (pane ids per area plus
the hidden set) is saved in localStorage under `loom.board.layout`; hiding a pane removes it
from the grid and the remaining panes fill the space (wide panes take a row, narrow ones pair up,
an odd last one widens). A saved layout is reconciled with the registry: unknown ids drop, new
panes append; malformed JSON is an error and the default layout is used.

### Adding a pane or a tab

Panes (`src/lib/board/panes/`):

1. Write `panes/<Name>.svelte`. Its props are exactly what its `select` returns plus
   `PaneShared` (`onselect(selection | null)`, `client`); it never reads the whole model.
2. Put the descriptor in `panes/<name>.ts` beside it: `definePane({ id, title, icon, area:
   "rail" | "main", shows, wide?, select: (model, selection) => props, component })`. Overview
   panes use `shows: overview`; a Detail pane uses `shows: detailOf("def" | "actor" | "build" |
   "run")` and throws from `select` when the selection is another kind.
3. Add one import and one line to the list in `panes/index.ts`. `tests/board/registry.test.ts`
   checks unique ids and runs every `select` against a fixture model.

Detail tabs (`src/lib/board/detail/`):

1. Write `detail/<Name>Tab.svelte`; its props are what its `select` returns.
2. Add one `defineTab({ id, title, applies: (selection) => boolean, select: (context) => props,
   component })` entry to `detail/tabs.ts`. `DetailContext` is the union a Detail pane builds
   (`def` with its loaded `view`, `actor`, `build`); `select` narrows it. The segmented control
   (`data-tab=<id>`) and the default tab (the first applicable) come from the registry order.
3. Nothing else: `DetailTabs.svelte` draws whatever `tabsFor(selection)` returns.

### Data path (`src/lib/board/`)

- `connect.ts` pages `GET /v1/events?after=<seq>&limit=1000` from 0 until a page is short, then
  opens `/v1/stream`, sends `{token, after}` and folds every `{seq, ts, event}` frame; frames
  without `seq` (actor table subscriptions) are ignored. A closed socket reconnects after
  1, 2, 4, 8 then 10 seconds and re-snapshots from the last seq. The header badge
  (`data-testid="board-status"`) reads `live` while the socket is open; every failure is a red
  strip with the error text. `text(hash)` (CAS) and `wasm(componentHash)` are fetched once per
  hash for the client's lifetime; a failed fetch is retried on the next request.
- `feed.ts` is the reducer: `defined` creates or updates a definition (name, lang, exports,
  effect labels, deps, component hash); `component_built` creates a build keyed by component
  hash and joined to its definition at render time; `call_completed` lights the definition
  for 1.5 s; `actor_message` moves an actor's cursor at once and the tree poll reconciles;
  unknown types are feed rows only. The feed keeps the newest 500 rows. A replayed seq is dropped.
  `runOf`, `dependentsOf` and `targetOfRow` serve the Details and the feed's click targets.
- `layout.ts` layers the graph by longest path from dependencies (leftmost) to dependents,
  orders each column by name, and places synthetic `isolated call` (`call` label, dashed red)
  and `host` (every other label, dotted) targets in the last column. No overlaps by construction.
- `stages.ts` reads a build log from `GET /v1/cas/<logs_ref>` and sums every `build_stages`
  object; the bars are sorted by milliseconds, named stages in `--series-a`, `unattributed_ms`
  in `--series-b`.
- `wasm.ts` validates `GET /v1/wasm/<hash>`, indexes the line map both ways, and classifies
  functions as own or external; `source.ts` decides which source text a view shows.

The actors pane polls `POST /v1/command {"command":"tree"}` every second while the page is
visible and pauses when hidden; a changed cursor highlights the row for 1.5 s. Keys: click opens
a Detail, `Esc` returns, `j`/`k` move through the focused pane's rows, `/` focuses the feed
filter, drag pans and the wheel zooms the graph (`0` or the fit button fits it), `+`/`-` change
the type scale, `?` shows these hints. DOM hooks for end-to-end scripts: `board-status`,
`board-error`, `board-overview`, `board-detail` (`data-kind`, `data-hash`), `board-def`
(`data-hash`, `data-name`), graph edges (`data-kind`, `data-from`, `data-to`), `board-event`
(`data-type`, `data-seq`), `board-actor` (`data-id`, `data-cursor`), `board-build` (`data-hash`,
bars `data-stage`), `detail-back`, `detail-dep`, `detail-source`, `detail-wasm`,
`detail-actor`, `detail-build`, `detail-run`, `detail-stages`, `detail-log`, `workspace-link`.

```sh
cd ui && bun test tests/board
```

The board tests cover the reducer, the layout, stage parsing, the connection (injected fetch and
a fake socket), the selection fragment, the wat line map and function classification, the
formatted-source fallback, and the pane and tab registries with the saved arrangement. They were
written in a write-only lane and have not been run.

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

From this directory:

```sh
bun test tests/bind
```

The five tests use isolated `happy-dom` 20.14.3 windows with MutationObserver and
focus/selection APIs. Existing UI tests had no DOM shim. This dependency is pinned
in `package.json`; the lead must update `ui/bun.lock` and install it before the gate.
No dependency installation or test execution ran in the write-only implementation lane.
