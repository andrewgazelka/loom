import { json } from "@sveltejs/kit";
import { env } from "$env/dynamic/private";
import type { RequestHandler } from "./$types";

// Re-run an already built cell by its hash: the animation loop calls this once per frame.
const url = env.LOOM_URL ?? "http://127.0.0.1:8850";
const token = env.LOOM_TOKEN ?? "replbench";

export const POST: RequestHandler = async ({ request }) => {
  const { target, args, sites } = await request.json();
  const reply = await fetch(`${url}/v1/command`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({ command: "run", args: { target, args, ...(sites ? { sites: true } : {}) } }),
  });
  return json(await reply.json());
};
