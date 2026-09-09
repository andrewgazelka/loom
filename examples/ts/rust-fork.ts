import { fork, join } from "loom";
import { rustAdd } from "loom:defs";
export default function parallel(left: number, right: number): number[] {
  return join([fork(rustAdd, [left, right]), fork(rustAdd, [right, left])]);
}
