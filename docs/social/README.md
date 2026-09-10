# Launch assets

- [Numbered posts, plain text](launch-thread.txt)
- [Recursive file-listing example](async-example.rs)
- [Complete recursive text-search example](fork-join-example.rs), also [plain text](fork-join-example.txt)
- [Dark PNG](../assets/loom-grep-dark.png) and [vector SVG](../assets/loom-grep-dark.svg)

The current graphic is rendered from code, with Berkeley Mono and actual contextual programming ligatures. It shows a recursive filesystem walk using an ordinary Rust function. The caller supplies the machine name and root path. The walk is bounded to depth 64 and 10,000 entries; it returns file paths relative to the requested root. The example uses `unwrap()` to keep error handling out of the illustration. It is not a benchmark result. The older image-generated graphic is a previous draft.

To regenerate from the Rust source:

```sh
uv run --locked scripts/render-social.py
```

The renderer needs `rsvg-convert` and locally installed `BerkeleyMono-Regular.otf` and `BerkeleyMono-Bold.otf`. Use `--font-dir /path/to/fonts` to change the default `~/Library/Fonts`. HarfBuzz shapes the text with `liga` and `calt` enabled. The SVG contains glyph outlines, so viewing the graphic does not require the font. Font files are not redistributed.
