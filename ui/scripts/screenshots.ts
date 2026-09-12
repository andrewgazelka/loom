import { mkdir } from "node:fs/promises";
import { commands } from "../src/lib/workbench/commands";
const origin = process.env.UI_PREVIEW_URL ?? "http://127.0.0.1:5186";
const output = new URL("../screenshots/r5-ui/", import.meta.url).pathname;
await mkdir(output, { recursive: true });
async function browser(args: string[]): Promise<string> {
  const child = Bun.spawn(["agent-browser", "--session", "r5-ui", ...args], {
    stdout: "pipe",
    stderr: "pipe",
  });
  const stdout = await new Response(child.stdout).text();
  const stderr = await new Response(child.stderr).text();
  const code = await child.exited;
  if (code !== 0)
    throw new Error(`agent-browser ${args[0]} rc=${code}: ${stderr || stdout}`);
  return stdout;
}
await browser(["set", "viewport", "1440", "1000"]);
await browser(["set", "media", "dark"]);
await browser(["open", `${origin}/?mock=1&panel=add`]);
await browser(["wait", 'section[aria-label="add result"]']);
await browser([
  "eval",
  `document.querySelector('.cm-content[aria-label="Rust source"]').focus()`,
]);
await browser(["press", "Meta+Enter"]);
await browser([
  "eval",
  `(() => { if(document.querySelectorAll('.repl-history article').length !== 2) throw new Error('Cmd+Enter must execute exactly once'); })()`,
]);
console.log("test result: ok. CodeMirror Cmd+Enter executes once");
await browser(["press", "Escape"]);
await browser([
  "eval",
  `(() => { if(!document.activeElement.closest('[data-pane="explorer"]')) throw new Error('Editor Escape must focus panel list'); document.querySelector('.repl-history [data-row]').focus(); })()`,
]);
console.log("test result: ok. editor Esc focuses panel list");
await browser(["press", "r"]);
await browser([
  "eval",
  `(() => { if(document.querySelectorAll('.repl-history article').length !== 3) throw new Error('History rerun must append exactly one result'); if(document.querySelectorAll('.cm-lineNumbers').length !== 2) throw new Error('Missing editor line numbers'); })()`,
]);
console.log("test result: ok. history r reruns exactly once");
await browser(["screenshot", `${output}editor.png`]);
let passed = 0;
for (const command of commands) {
  await browser(["open", `${origin}/?mock=1&panel=${command.id}`]);
  await browser(["wait", `section[aria-label="${command.name} result"]`]);
  await browser([
    "eval",
    `(() => { const error = document.querySelector('.error'); if (error) throw new Error(error.textContent); return document.fonts.ready.then(() => 'ready'); })()`,
  ]);
  await browser(["screenshot", `${output}${command.id}.png`]);
  passed++;
  console.log(
    `test result: ok. mock panel ${command.id} (${passed}/${commands.length})`,
  );
}
for (const verdict of ["Matched", "Differs", "Trapped"]) {
  await browser([
    "open",
    `${origin}/?mock=1&panel=actor_validate&verdict=${verdict}`,
  ]);
  await browser(["wait", 'section[aria-label="actor_validate result"]']);
  await browser(["screenshot", `${output}validate-${verdict}.png`]);
}
await browser(["set", "media", "light"]);
await browser(["open", `${origin}/?mock=1&panel=view`]);
await browser(["wait", 'section[aria-label="view result"]']);
await browser(["screenshot", `${output}view-light.png`]);
await browser(["set", "media", "dark"]);
console.log(
  `test result: ${passed}/${commands.length} mock panels render; screenshots: ${output}`,
);
