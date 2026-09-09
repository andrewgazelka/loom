import { record } from "./api";
/** Display the protocol's Value shapes without guessing language-specific types. */
export function shapeLabel(value: unknown, depth = 0): string {
  const shape = record(value);
  if (depth > 4) return "…";
  switch (shape.type) {
    case "null": case "boolean": case "number": case "string": return shape.type;
    case "value": return "Value";
    case "array": return `Array<${shapeLabel(shape.items, depth + 1)}>`;
    case "ref": return `Ref<${shapeLabel(shape.target, depth + 1)}>`;
    case "object": {
      const optional = Array.isArray(shape.optional) ? shape.optional : [];
      const properties = record(shape.properties);
      return `{ ${Object.keys(properties).map(name => `${name}${optional.includes(name) ? "?" : ""}: ${shapeLabel(properties[name], depth + 1)}`).join("; ")} }`;
    }
    default: return "unknown";
  }
}
export function signatures(value: unknown, enclosingName = ""): string[] {
  const exports = record(value).exports;
  if (!Array.isArray(exports)) return [];
  return exports.map(item => {
    const entry = record(item);
    const params = Array.isArray(entry.params) ? entry.params.map(param => {
      const data = record(param);
      return `${typeof data.name === "string" && data.name ? `${data.name}: ` : ""}${shapeLabel(data.shape)}`;
    }).join(", ") : "…";
    const name = typeof entry.name === "string" && !(enclosingName && (entry.name === "main" || entry.name === enclosingName)) ? entry.name : "";
    return `${name}(${params}) → ${shapeLabel(entry.returns)}`;
  });
}
