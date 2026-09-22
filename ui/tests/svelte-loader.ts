/**
 * `bun test` preload (see `bunfig.toml`): compile `.svelte` files on import so tests can load the
 * pane and tab registries, which reference components. Components are only imported, never
 * mounted, by the unit tests; DOM behaviour is covered by the browser scripts.
 */
import { plugin } from "bun";
import { compile } from "svelte/compiler";

plugin({
  name: "svelte",
  setup(build) {
    build.onLoad({ filter: /\.svelte$/ }, async ({ path }) => {
      const source = await Bun.file(path).text();
      const { js } = compile(source, { filename: path, generate: "client", css: "external" });
      return { contents: js.code, loader: "js" };
    });
  },
});
