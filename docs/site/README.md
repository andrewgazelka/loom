# Loom documentation site

```sh
bun install
bun run check
bun test
bun run build
bun run dev
```

The development server uses `http://127.0.0.1:5177` and refuses to choose a different port. Static HTML is written to `build/`; deploy that directory with directory-index support. No Node server is needed in production.

`src/lib/data/` owns repository inputs, parsing, rendering, navigation, and search. Vite imports `docs/*.md`, the root README, crate manifests, and Rust module documentation as raw text at build time. `src/lib/ui/` owns presentation and accepts typed data. Routes connect those layers.

Add a Markdown file directly under `docs/` with an H1 heading, or YAML front matter containing a `title`. Optional front matter fields are `slug` and `description`. Slugs use lowercase letters, digits, and hyphens. Unassigned pages appear in Reference. Architecture and content-addressed-code pages are optional while their source work is in progress; absent pages produce build warnings. Missing MCP source sections also warn and render an explicit empty state. Invalid metadata, TOML, or MCP tool rows fail with the source filename.

The MCP parser accepts a two-column Tool / Description table or backtick-quoted tool names in bullets under an `MCP surface` heading in `docs/actors-turso.md`. Code fences use Shiki; Mermaid fences render in the browser with the dark theme. Markdown remains the content source. Relative document links become site routes, while repository source links and images use the repository's GitHub origin.

The interface follows the OS color scheme. `j` / `k` move within a pane, `h` / `l` switch panes, Cmd+K or Ctrl+K opens search, `?` shows keyboard help, and `+` / `-` adjusts type size. Search runs locally over document headings and paragraphs. On narrow screens, the navigation toggle opens the document tree.
