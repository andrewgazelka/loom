// A real build/workspace/index.html for the deep link (`/workspace/?panel=view&target=…`), like
// the root page; ServeDir redirects /workspace to /workspace/ and keeps the query and fragment.
export const prerender = true;
export const trailingSlash = "always";
