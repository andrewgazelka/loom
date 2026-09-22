// loomd serves ui/build with tower-http ServeDir and no SPA fallback: a deep link needs a real
// file. Prerendering the client-only shell writes build/board/index.html, and ServeDir redirects
// /board to /board/ (the browser keeps the #token= fragment across that redirect).
export const prerender = true;
export const trailingSlash = "always";
