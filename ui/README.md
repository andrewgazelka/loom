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

The development server proxies `/api` to `127.0.0.1:8787`. Connection settings
accept an optional HTTP endpoint and bearer token. Manual connections and launcher fragment tokens are saved as `{ endpoint, token }`
in localStorage under `loom.connection`. Open `/#token=<URL-encoded token>` to
connect to the page origin automatically; the fragment is removed before requests.
Query-string tokens are rejected because they reach server logs. The
static build goes to `build/`; production must serve `index.html` and route
`/api` to the daemon. HTTP errors remain visible with operation context.

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
pills show eight characters and copy the full hash. Session history retains
command inputs, results, and errors in invocation order; background workspace
refreshes are excluded. History is in memory and clears on page reload.

## Visual verification environment

The addendum's CDP connection through `repl.ts` reached the browser WebSocket but
failed with `browserType.connectOverCDP: Timeout 30000ms exceeded`. Visual checks
therefore use the permitted `agent-browser --session r5-ui` fallback.


Computer Use startup failed while requesting
`sky.get_app_state({app:'com.google.Chrome'})` with the verbatim error
`Sky Computer Use native pipe startup failed`. This blocks Computer Use UI checks
in this session. The owner's explicitly requested
`agent-browser --session r5-ui` provides the screenshot and interaction route.

With the development server running, `bun scripts/screenshots.ts` verifies a
rendered result for all 30 panels and writes PNGs under `screenshots/r5-ui/`.
It also captures Matched, Differs and Trapped validation results and the OS-light
stylesheet. Set `UI_PREVIEW_URL` to verify a locally served production build.

Restart `vite preview` after rebuilding. In this SvelteKit setup a running preview
retained the earlier entry script even after `build/index.html` changed. Restarting
made the served entry match the built entry and the changed actor-selection
behavior pass. Keep the preview build fixed while taking screenshots.
