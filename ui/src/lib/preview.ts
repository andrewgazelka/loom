let active = 0;
const waiting: (() => void)[] = [];
export async function boundedPreview<T>(load: () => Promise<T>): Promise<T> {
  if (active >= 3) await new Promise<void>(resolve => waiting.push(resolve));
  else active++;
  try { return await load(); }
  finally { const next = waiting.shift(); if (next) next(); else active--; }
}
