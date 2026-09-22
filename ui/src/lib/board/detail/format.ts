/** Small formatters shared by the Detail panes and tabs. */

/** `HH:MM:SS` in the browser's zone; `ts` is Unix seconds. */
export function clock(ts: number): string {
  const date = new Date(ts * 1000);
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

/** `YYYY-MM-DD HH:MM:SS` in the browser's zone; `ts` is Unix seconds. */
export function stamp(ts: number): string {
  const date = new Date(ts * 1000);
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${clock(ts)}`;
}

export function bytes(size: number): string {
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KiB`;
  return `${(size / (1024 * 1024)).toFixed(2)} MiB`;
}

export const HASH = /^[a-f0-9]{64}$/;
export const short = (hash: string) => hash.slice(0, 8);

/** The message of any thrown value. */
export function reason(problem: unknown): string {
  return problem instanceof Error ? problem.message : String(problem);
}

/** Quote an SQLite identifier. */
export function identifier(name: string): string {
  return `"${name.replaceAll('"', '""')}"`;
}
