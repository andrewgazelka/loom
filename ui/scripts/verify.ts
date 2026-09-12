const checks = [
  { name: "typecheck", args: ["bun", "run", "check"] },
  { name: "production build", args: ["bun", "run", "build"] },
  { name: "contracts and state", args: ["bun", "test", "tests"] },
];
let passed = 0;
let firstFailure = "none";
for (const check of checks) {
  const process = Bun.spawn(check.args, {
    stdout: "inherit",
    stderr: "inherit",
  });
  const code = await process.exited;
  if (code === 0) passed++;
  else if (firstFailure === "none") firstFailure = check.name;
  console.log(
    `test result: ${code === 0 ? "ok" : "FAILED"}. ${check.name} rc=${code}`,
  );
}
console.log(
  `test result: ${passed}/3 UI gates pass; first failing step: ${firstFailure}`,
);
process.exit(passed === checks.length ? 0 : 1);
