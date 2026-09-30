import { json } from "@sveltejs/kit";
import { env } from "$env/dynamic/private";
import type { RequestHandler } from "./$types";

// The browser never sees the daemon token: this route forwards one `eval` command.
const url = env.LOOM_URL ?? "http://127.0.0.1:8850";
const token = env.LOOM_TOKEN ?? "replbench";

export const POST: RequestHandler = async ({ request }) => {
  const { source, manifest, lock, entry, deps, args: callArgs } = await request.json();
  const args: Record<string, unknown> = { source };
  if (deps) Object.assign(args, { deps });
  if (entry) Object.assign(args, { entry, args: callArgs ?? [] });
  if (manifest && lock) Object.assign(args, { manifest, lock });
  const started = performance.now();
  const reply = await fetch(`${url}/v1/command`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({ command: "eval", args }),
  });
  const body = await reply.json();
  return json({ wall_ms: Math.round(performance.now() - started), ...body });
};
