import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";

// HTTP delivery checks complement the native Computer Use rendering witness.
// This script does not claim that fetching a bundle proves browser execution.
let daemon: ReturnType<typeof Bun.spawn> | undefined;
let scratch: string | undefined;
let diagnostics = "";
let url = process.env.LOOM_URL;
try {
  if (!url) {
    const binary = process.env.LOOMD_BINARY;
    if (!binary)
      throw new Error("LOOMD_BINARY is required when LOOM_URL is unset");
    scratch = await mkdtemp(join(tmpdir(), "loom-ui-smoke-"));
    daemon = Bun.spawn(
      [
        resolve(binary),
        "--root",
        process.cwd(),
        "--db",
        join(scratch, "loom.sqlite"),
        "--bind",
        "127.0.0.1:0",
        "--token",
        "loom-ui-smoke",
      ],
      { stdout: "pipe", stderr: "pipe" },
    );
    const processHandle = daemon;
    const listening = (async () => {
      for await (const chunk of processHandle.stderr as ReadableStream<Uint8Array>) {
        diagnostics += new TextDecoder().decode(chunk);
        const address = diagnostics.match(
          /loomd listening on (127\.0\.0\.1:\d+)/,
        )?.[1];
        if (address) return `http://${address}`;
      }
      throw new Error(`Daemon exited before listening: ${diagnostics}`);
    })();
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      url = await Promise.race([
        listening,
        new Promise<never>((_, reject) => {
          timer = setTimeout(
            () => reject(new Error(`Daemon startup timed out: ${diagnostics}`)),
            15000,
          );
        }),
      ]);
    } finally {
      if (timer) clearTimeout(timer);
    }
  }
  const origin = new URL(url);
  const indexResponse = await fetch(origin);
  if (
    !indexResponse.ok ||
    !indexResponse.headers.get("content-type")?.includes("text/html")
  )
    throw new Error(
      `UI document returned HTTP ${indexResponse.status} with ${indexResponse.headers.get("content-type")}`,
    );
  const servedIndex = new Uint8Array(await indexResponse.arrayBuffer());
  const builtIndex = await readFile("loom-ui/build/index.html");
  if (!Buffer.from(servedIndex).equals(builtIndex))
    throw new Error("Daemon UI document differs from the current static build");
  const html = new TextDecoder().decode(servedIndex);
  const assets = [
    ...new Set(
      html.match(/(?:\.\/|\/)\_app\/immutable\/[^\s"'`<>]+?\.js/g) ?? [],
    ),
  ];
  if (assets.length < 2)
    throw new Error("UI document is missing Svelte startup bundles");
  for (const asset of assets) {
    const assetUrl = new URL(asset, origin);
    const response = await fetch(assetUrl);
    if (
      !response.ok ||
      !response.headers.get("content-type")?.includes("javascript")
    )
      throw new Error(
        `Startup bundle ${asset} returned HTTP ${response.status}`,
      );
    const served = Buffer.from(await response.arrayBuffer());
    const built = await readFile(
      join("loom-ui/build", asset.replace(/^\.?\//, "")),
    );
    if (!served.equals(built))
      throw new Error(`Startup bundle ${asset} differs from the current build`);
  }
  console.log(
    `M10: static document and ${assets.length} startup bundles match daemon delivery. Browser execution and gestures require the separate native UI check.`,
  );
} finally {
  if (daemon) {
    daemon.kill("SIGINT");
    const shutdown = await Promise.race([
      daemon.exited.then(() => true),
      Bun.sleep(2000).then(() => false),
    ]);
    if (!shutdown) {
      daemon.kill("SIGKILL");
      await daemon.exited;
    }
  }
  if (scratch) await rm(scratch, { recursive: true, force: true });
}
