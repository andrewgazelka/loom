import { json, error } from "@sveltejs/kit";
import { env } from "$env/dynamic/private";
import type { RequestHandler } from "./$types";

// The definition verbs the page may call. Everything else stays behind the token.
const allowed = new Set(["add", "update", "history", "view", "diff", "dependents"]);
const url = env.LOOM_URL ?? "http://127.0.0.1:8850";
const token = env.LOOM_TOKEN ?? "replbench";

export const POST: RequestHandler = async ({ request }) => {
  const { command, args } = await request.json();
  if (!allowed.has(command)) error(400, `command ${command} is not exposed`);
  const reply = await fetch(`${url}/v1/command`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({ command, args }),
  });
  return json(await reply.json());
};
