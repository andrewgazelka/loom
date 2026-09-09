import { call } from "loom";
import { rustAdd } from "loom:defs";
export default function invoke(left: number, right: number): number {
  return call(rustAdd, [left, right]);
}
