/** Build logs are JSON lines; the lines carrying a `build_stages` object attribute the build's milliseconds to named stages. */

/** Sum every `build_stages` object in the log by stage name; non-JSON lines and other objects are ignored. */
export function parseStages(log: string): Record<string, number> {
  const stages: Record<string, number> = {};
  for (const line of log.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed.startsWith("{")) continue;
    let value: unknown;
    try {
      value = JSON.parse(trimmed);
    } catch {
      continue;
    }
    if (typeof value !== "object" || value === null) continue;
    const found = (value as Record<string, unknown>).build_stages;
    if (typeof found !== "object" || found === null || Array.isArray(found))
      continue;
    for (const [name, ms] of Object.entries(found as Record<string, unknown>))
      if (typeof ms === "number" && Number.isFinite(ms))
        stages[name] = (stages[name] ?? 0) + ms;
  }
  return stages;
}

/** Stages by descending milliseconds, ties by name, for the bar chart. */
export function sortedStages(stages: Record<string, number>): [string, number][] {
  return Object.entries(stages).sort((a, b) =>
    b[1] !== a[1] ? b[1] - a[1] : a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0,
  );
}
