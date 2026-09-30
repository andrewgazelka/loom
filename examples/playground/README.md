# Loom playground

A SvelteKit page over a running `loomd`. A cell draws by performing effects, not by calling a
canvas: `canvas2d.line`, `canvas2d.circle`, `canvas3d.line` and `canvas3d.tri` are plain
`loom::perform` labels, and `collect` (the `canvas.rs` tab) is a guest handler that turns them
into a scene. The page paints that scene. Swap the handler and the same drawing code means
something else (SVG, a bounding box, a count).

- `spirograph`: 2D, animated. The page builds the cell once, then calls `run` with the clock
  (milliseconds, a `u32`: an `f64` parameter rejects a JSON integer such as `0`) every frame.
- `terrain`, `torus`: 3D. Rust builds the mesh once; dragging orbits the camera in the page,
  scroll zooms, and it spins until touched. `torus` uses glam through a pinned lock.

```sh
# daemon (see docs/guide.md), then:
cd examples/playground && bun install && LOOM_URL=http://127.0.0.1:8787 LOOM_TOKEN=... bun run dev
```

`src/routes/api/{eval,run}/+server.ts` forward one command each, so the token stays on the server.
