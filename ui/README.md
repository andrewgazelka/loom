# Loom REPL UI

A static SvelteKit workspace for Rust definitions and durable actors. Definitions
and all 19 MCP actor operations use the HTTP contract in [API.md](API.md).

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
