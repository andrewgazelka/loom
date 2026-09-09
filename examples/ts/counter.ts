export function init(): number { return 0; }
export function run(_state: number, msg: number): number[] { return [msg]; }
export function fold(state: number, event: number): number { return state + event; }
