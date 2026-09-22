/** One connection store for every page: a `#token=` fragment (consumed, saved, removed from the URL) or the saved `loom.connection`. */
import { fragmentToken } from "./fragment-token";

export const connectionKey = "loom.connection";
export interface Connection {
  endpoint: string;
  token: string;
}
export interface ResolvedConnection extends Connection {
  source: "fragment" | "saved";
}
export class QueryTokenError extends Error {
  constructor() {
    super(
      "Query-string tokens are not accepted; use #token=… because ?token= reaches server logs.",
    );
    this.name = "QueryTokenError";
  }
}
export interface ConnectionEnvironment {
  location: { hash: string; search: string; pathname: string };
  history: { state: unknown; replaceState(data: unknown, unused: string, url?: string): void };
  storage: Pick<Storage, "getItem" | "setItem">;
}

export function savedConnection(storage: Pick<Storage, "getItem">): Connection | null {
  const saved = storage.getItem(connectionKey);
  if (saved === null) return null;
  const connection: unknown = JSON.parse(saved);
  const row = connection as Record<string, unknown> | null;
  if (!row || typeof row.endpoint !== "string" || typeof row.token !== "string")
    throw new Error("Saved connection: expected endpoint and token strings.");
  return { endpoint: row.endpoint, token: row.token };
}

export function saveConnection(storage: Pick<Storage, "setItem">, connection: Connection) {
  storage.setItem(connectionKey, JSON.stringify(connection));
}

/**
 * Remove any `#token=` fragment from the URL before parsing or requesting anything, reject
 * `?token=`, then return the fragment token (saved against the page origin) or the saved connection.
 */
export function resolveConnection(
  environment: ConnectionEnvironment,
  options: { readSaved?: boolean } = {},
): ResolvedConnection | null {
  const { location, history, storage } = environment;
  const fragment = location.hash;
  if (new URLSearchParams(fragment.slice(1)).has("token"))
    history.replaceState(history.state, "", location.pathname + location.search);
  if (new URLSearchParams(location.search).has("token")) throw new QueryTokenError();
  const token = fragmentToken(fragment);
  if (token !== null) {
    const connection = { endpoint: "", token };
    saveConnection(storage, connection);
    return { ...connection, source: "fragment" };
  }
  if (options.readSaved === false) return null;
  const saved = savedConnection(storage);
  return saved === null ? null : { ...saved, source: "saved" };
}
