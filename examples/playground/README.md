# Loom playground

A SvelteKit page over a running `loomd`: type a Rust cell, press Run, see the output, the
timings and, for the torus preset, a mesh that glam computed in the guest.

```sh
# daemon (see docs/guide.md), then:
cd examples/playground && bun install && LOOM_URL=http://127.0.0.1:8787 LOOM_TOKEN=... bun run dev
```

`src/routes/api/eval/+server.ts` forwards one `eval` command, so the token stays on the server.
