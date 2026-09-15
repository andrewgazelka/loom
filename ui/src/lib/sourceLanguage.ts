export function sourceLanguage(value: unknown): "typescript" | "javascript" | "rust" {
  if (value === "rust" || value === "javascript" || value === "typescript") return value;
  throw new Error(`Unsupported source language: ${String(value)}`);
}
