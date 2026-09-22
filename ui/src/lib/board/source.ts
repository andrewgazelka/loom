/**
 * Which text a definition's Source view shows. The `view` verb returns the stored bytes as
 * `source` and, for Rust, a rustfmt rendering as `formatted_source` with `format_error` naming
 * why formatting failed when it is null. The formatted text is the default; the stored bytes
 * appear only behind the "as submitted" toggle.
 */
export interface SourceFields {
  lang: string;
  source: string;
  formatted_source: string | null;
  format_error: string | null;
}

export interface DisplayedSource {
  code: string;
  /** One muted line under the header, or `null` when there is nothing to say. */
  note: string | null;
  /** True when `code` is the rustfmt rendering. */
  formatted: boolean;
}

export function displayedSource(view: SourceFields, asSubmitted: boolean): DisplayedSource {
  if (asSubmitted)
    return { code: view.source, note: "Exact stored bytes, as submitted.", formatted: false };
  if (view.formatted_source !== null)
    return { code: view.formatted_source, note: null, formatted: true };
  if (view.lang !== "rust") return { code: view.source, note: null, formatted: false };
  return {
    code: view.source,
    note: `Not formatted: ${view.format_error ?? "the server gave no reason"}`,
    formatted: false,
  };
}

/** Lines of `code`, one entry per line; a trailing newline does not add an empty line. */
export function splitLines(code: string): string[] {
  const lines = code.split("\n");
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  return lines;
}
