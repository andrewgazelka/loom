import { json } from "@sveltejs/kit";
import type { RequestHandler } from "./$types";

// The browser never sees the daemon token: this route forwards one `eval` command.
const url = process.env.LOOM_URL ?? "http://127.0.0.1:8850";
const token = process.env.LOOM_TOKEN ?? "replbench";

export const POST: RequestHandler = async ({ request }) => {
  const { source, manifest, lock } = await request.json();
  const args: Record<string, string> = { source };
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
