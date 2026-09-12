# ui

The Loom workspace is a static SvelteKit app. It uses only the `/v1` API and authenticates the event stream in its first WebSocket message.

```sh
bun install --frozen-lockfile
bun run check
bun run dev
```

The development server proxies HTTP and WebSocket requests to `127.0.0.1:8787`. Enter the daemon's bearer token in Connection settings. The endpoint defaults to the current origin; production should serve the app and API together. Settings and session identity persist in browser local storage.

`bun run build` writes static assets to `build/`. Configure the static server to serve `index.html` for app routes and forward `/v1` to loomd.

The session prompt supports TypeScript evaluation, TypeScript/Rust definitions with dependency hashes, and JSON commands. Definitions, actor state, component build logs, and dependency graphs read the same protocol. Large result references resolve through CAS. Graph navigation uses trackpad scrolling and pointer-centered pinch zoom; keyboard navigation is documented in the app's help.
