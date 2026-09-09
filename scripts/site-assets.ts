/** Read-only production static-delivery regression shared by site and Nix gates. */
export async function validateSiteAssets(endpoint: string): Promise<void> {
  const read = (url: string | URL, headers?: HeadersInit) => fetch(url, { headers, signal: AbortSignal.timeout(15_000) });
  const document = await read(endpoint);
  if (document.status !== 200 || !document.headers.get('content-type')?.includes('text/html')) throw new Error('UI document not served');
  if (!document.headers.get('cache-control')?.includes('no-store')) throw new Error('UI document must use no-store across deployments');
  const html = await document.text();
  for (const date of ['Thu, 01 Jan 1970 00:00:01 GMT', 'Fri, 01 Jan 2100 00:00:00 GMT']) {
    const reload = await read(endpoint, { 'If-Modified-Since': date, 'If-None-Match': '"previous-package"' });
    if (reload.status !== 200 || await reload.text() !== html) throw new Error(`UI conditional reload retained stale HTML (${date})`);
  }
  const scripts = new Set([
    ...Array.from(html.matchAll(/(?:src|href)=["']([^"'\s]+\.js(?:\?[^"']*)?)["']/g), match => match[1]!),
    ...Array.from(html.matchAll(/import\(["']([^"'\s]+\.js(?:\?[^"']*)?)["']\)/g), match => match[1]!),
  ]);
  if (scripts.size === 0) throw new Error('UI document has no JavaScript entry');
  await Promise.all(Array.from(scripts, async script => {
    const url = new URL(script, endpoint);
    const asset = await read(url);
    const mime = asset.headers.get('content-type') ?? '';
    if (asset.status !== 200 || (!mime.includes('javascript') && !mime.includes('ecmascript')) || !(await asset.text()).length) throw new Error(`UI startup module not served: ${script} (HTTP ${asset.status})`);
    if (url.pathname.startsWith('/_app/immutable/')) {
      const cache = asset.headers.get('cache-control') ?? '';
      if (!cache.includes('immutable') || !cache.includes('max-age=31536000')) throw new Error(`Hashed asset lacks immutable cache policy: ${script}`);
    }
  }));
}
