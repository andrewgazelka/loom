import { loadCrates } from '$lib/data/crates';
export function load() { return { crates: loadCrates() }; }
