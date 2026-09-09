import { cas, perform, type Value } from "loom";

export default function dag(payload: Value, target: string): Value {
  if (target === "") return payload;
  if (payload === null || typeof payload !== "object" || Array.isArray(payload)) {
    throw new Error("payload must be a link");
  }
  const hash = payload["$ref"];
  if (typeof hash !== "string") throw new Error("payload must be a link");
  const value = cas.get({ hash });
  const echo = perform({ op: "call", args: { def: target, args: [payload, ""] } });
  return { reference: payload, value, echo };
}
