/**
 * What the board has in focus, mirrored in the URL fragment once the `#token=` fragment has
 * been consumed: `#def=<definition hash>`, `#actor=<actor id>`, `#build=<component hash>`,
 * `#run=<trace scope>`. No fragment (or a fragment with no selection key) is the Overview.
 */
export type Selection =
  | { kind: "def"; hash: string }
  | { kind: "actor"; id: string }
  | { kind: "build"; hash: string }
  | { kind: "run"; scope: string };

const KINDS = ["def", "actor", "build", "run"] as const;
const HASH = /^[a-f0-9]{64}$/;

/**
 * Parse a `location.hash`. Keys other than the four selection keys (notably `token`) are
 * ignored; two selection keys at once, an empty value, or a malformed hash are errors so a
 * bad link never silently opens the Overview.
 */
export function parseSelection(hash: string): Selection | null {
  const params = new URLSearchParams(hash.replace(/^#/, ""));
  const present = KINDS.filter((kind) => params.has(kind));
  if (present.length === 0) return null;
  if (present.length > 1)
    throw new Error(`selection: one of ${KINDS.join(", ")} at a time, got ${present.join(" and ")}`);
  const kind = present[0]!;
  const values = params.getAll(kind);
  if (values.length !== 1) throw new Error(`selection: ${kind} given ${values.length} times`);
  const value = values[0]!;
  if (!value) throw new Error(`selection: ${kind} is empty`);
  switch (kind) {
    case "def":
      if (!HASH.test(value)) throw new Error(`selection: def is not a 64-hex definition hash`);
      return { kind, hash: value };
    case "build":
      if (!HASH.test(value)) throw new Error(`selection: build is not a 64-hex component hash`);
      return { kind, hash: value };
    case "actor":
      return { kind, id: value };
    case "run":
      return { kind, scope: value };
  }
}

/** The fragment for a selection, `#` included; the empty string for the Overview. */
export function formatSelection(selection: Selection | null): string {
  if (selection === null) return "";
  const params = new URLSearchParams();
  switch (selection.kind) {
    case "def":
    case "build":
      params.set(selection.kind, selection.hash);
      break;
    case "actor":
      params.set("actor", selection.id);
      break;
    case "run":
      params.set("run", selection.scope);
      break;
  }
  return `#${params.toString()}`;
}

export function sameSelection(a: Selection | null, b: Selection | null): boolean {
  return formatSelection(a) === formatSelection(b);
}
