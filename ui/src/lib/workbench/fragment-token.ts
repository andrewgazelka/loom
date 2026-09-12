/** URL fragments use form encoding; query parameters are deliberately not inputs. */
export function fragmentToken(hash: string): string | null {
  if (!hash.startsWith("#")) return null;
  const params = new URLSearchParams(hash.slice(1));
  const tokens = params.getAll("token");
  if (!tokens.length) return null;
  if (tokens.length !== 1 || !tokens[0]?.trim())
    throw new Error("Fragment token: expected one nonempty token.");
  return tokens[0];
}
