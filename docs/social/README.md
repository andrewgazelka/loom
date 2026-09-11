# Launch assets

- [Numbered posts, plain text](launch-thread.txt)
- [Concurrent timer example](async-example.rs)
- [Complete recursive text-search example](spawn-join-example.rs), also [plain text](spawn-join-example.txt)
- [Dark PNG](../assets/loom-grep-dark.png) and [vector SVG](../assets/loom-grep-dark.svg)

The source example spawns two scoped children that call `sleep(100)` and `sleep(200)`; the scope joins both before it returns, so the ordinary Rust function returns `()` after both timers finish. The SVG and PNG assets are rendered from that exact source. The image contains only code; the post supplies the headline. It is not a benchmark result. The older image-generated graphic is a previous draft.

To regenerate from the Rust source:

```sh
uv run --locked scripts/render-social.py
```

The renderer needs `rsvg-convert` and locally installed `BerkeleyMono-Regular.otf` and `BerkeleyMono-Bold.otf`. Use `--font-dir /path/to/fonts` to change the default `~/Library/Fonts`. HarfBuzz shapes the text with `liga` and `calt` enabled. The SVG contains glyph outlines, so viewing the graphic does not require the font. Font files are not redistributed.
