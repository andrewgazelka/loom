import { createHighlighterCore } from "shiki/core";
import { createOnigurumaEngine } from "shiki/engine/oniguruma";
import typescript from "@shikijs/langs/typescript";
import rust from "@shikijs/langs/rust";
import json from "@shikijs/langs/json";
import toml from "@shikijs/langs/toml";
import shellscript from "@shikijs/langs/shellscript";
import light from "@shikijs/themes/github-light";
import dark from "@shikijs/themes/github-dark";
export const highlighter = createHighlighterCore({
  themes: [light, dark],
  langs: [typescript, rust, json, toml, shellscript],
  engine: createOnigurumaEngine(import("shiki/wasm")),
});
export type CodeLanguage =
  | "typescript"
  | "rust"
  | "json"
  | "toml"
  | "shellscript"
  | "text";
