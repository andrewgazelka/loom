import { shapeLabel, signatures } from "../signature";
import { array, object, string, type Json } from "./schema";

export interface ArgumentHints {
  signatures: string[];
  example: string | null;
  note: string;
}
function exampleValue(value: unknown, depth = 0): Json {
  if (depth > 32) throw new Error("Argument shape is too deeply nested");
  const shape = object(value, "parameter shape");
  switch (shape.type) {
    case "null":
    case "value":
      return null;
    case "boolean":
      return false;
    case "number":
      return 0;
    case "string":
      return "example";
    case "array":
      return [];
    case "ref":
      return exampleValue(shape.target, depth + 1);
    case "object": {
      const properties = object(shape.properties, "shape.properties");
      const optional = array(shape.optional, "shape.optional");
      return Object.fromEntries(
        Object.keys(properties)
          .filter((name) => !optional.includes(name))
          .map((name) => [name, exampleValue(properties[name], depth + 1)]),
      );
    }
    default:
      throw new Error(`Cannot create an example for ${shapeLabel(shape)}`);
  }
}
export function argumentHints(value: unknown): ArgumentHints {
  const view = object(value, "definition view");
  const sig = object(object(view.def, "definition").sig, "signature");
  const exports = array(sig.exports, "signature.exports").map((value) =>
    object(value, "export"),
  );
  const labels = signatures(sig);
  const selected =
    typeof view.entry === "string"
      ? exports.find((entry) => entry.name === view.entry)
      : exports.length === 1
        ? exports[0]
        : undefined;
  if (typeof view.entry === "string" && !selected)
    throw new Error(
      `Selected entry ${view.entry} is absent from the signature`,
    );
  if (!selected)
    return {
      signatures: labels,
      example: null,
      note: exports.length
        ? "Choose an exported entry name as the target to see its argument example."
        : "This definition has no runnable exports.",
    };
  const name = string(selected.name, "entry.name");
  const params = array(selected.params, "entry.params");
  const example = params.map((param) =>
    exampleValue(object(param, "parameter").shape),
  );
  return {
    signatures: signatures({ exports: [selected] }),
    example: JSON.stringify(example, null, 2),
    note: `${name} takes ${params.length} ${params.length === 1 ? "argument" : "arguments"}. The outer array holds positional arguments; wrap an array argument in another array. Example values illustrate protocol shapes; the function may impose additional constraints.`,
  };
}
