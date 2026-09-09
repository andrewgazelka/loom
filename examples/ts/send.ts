import { send } from "loom";
interface Message { actor: string; value: number }
export function init(): number { return 0; }
export function run(_state: number, msg: Message): number[] {
  send(msg.actor, msg.value);
  return [msg.value];
}
export function fold(state: number, event: number): number { return state + event; }
