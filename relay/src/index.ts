export interface Env {
  ROOM: DurableObjectNamespace;
  RELAY_TOKEN: string;
}
export { Room2 } from "./room";

// RELAY_TOKEN may hold more than one secret, separated by commas, so a token
// can be rotated without a moment where the engine and the phone must switch
// together. Each secret is compared in constant time; a match against any of
// them is enough.
function bearerOk(header: string | null, tokens: string): boolean {
  if (!tokens || !header) return false;
  const prefix = "bearer ";
  if (header.toLowerCase().slice(0, prefix.length) !== prefix) return false;
  const given = header.slice(prefix.length).trim();
  if (!given) return false;
  const a = new TextEncoder().encode(given);
  let ok = false;
  for (const token of tokens.split(",").map((t) => t.trim()).filter(Boolean)) {
    const b = new TextEncoder().encode(token);
    let diff = a.length ^ b.length;
    const n = Math.max(a.length, b.length);
    for (let i = 0; i < n; i++) diff |= (a[i] ?? 0) ^ (b[i] ?? 0);
    if (diff === 0) ok = true;
  }
  return ok;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const { pathname } = new URL(request.url);
    if (pathname === "/healthz") return new Response("ok");

    const room = pathname.match(/^\/room\/([^/]+)$/);
    if (room) {
      if (!bearerOk(request.headers.get("authorization"), env.RELAY_TOKEN)) {
        return new Response("unauthorized", { status: 401 });
      }
      return env.ROOM.getByName(room[1]).fetch(request);
    }

    return new Response("not found", { status: 404 });
  },
} satisfies ExportedHandler<Env>;
