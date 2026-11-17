//! Public address of every engine instance: one hostname, one path segment per
//! engine.
//!
//! Request /<instance>/api/...  →  https://<instance>.<origin suffix>/api/...
//!
//! The first path segment names the instance; the Workers KV entry under that
//! name holds the origin hostname its own Named Tunnel answers on. The segment
//! is removed before the request is forwarded, so every engine keeps serving the
//! paths it has always served (/api/..., /ws, /healthz) and nothing in the engine
//! or the phone has to know it arrived through a shared address.

// The same shape the engine enforces (aixodia_state::valid_instance_name):
// 32 characters at most, alnum at both ends, single hyphens between.
const INSTANCE_NAME = /^[a-z0-9](?:[a-z0-9]|-[a-z0-9]){0,31}$/;
const ORIGIN_HOST = /^[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?\.aixodia\.dpdns\.org$/;

/// Headers a proxy must not forward in either direction.
const HOP_BY_HOP = new Set([
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
  "host"
]);

function error(status, message) {
  return new Response(JSON.stringify({ error: message }), {
    status,
    headers: { "content-type": "application/json; charset=utf-8", "cache-control": "no-store" }
  });
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const segments = url.pathname.split("/").filter(Boolean);
    const instance = segments[0] || "";
    if (!INSTANCE_NAME.test(instance)) return error(404, "Unknown instance");
    if (!env.ENGINE_ORIGINS) return error(503, "Gateway is not configured");
    const origin = await env.ENGINE_ORIGINS.get(instance);
    if (!origin || !ORIGIN_HOST.test(origin)) return error(404, "Unknown instance");
    if (!env.ACCESS_CLIENT_ID || !env.ACCESS_CLIENT_SECRET) {
      return error(503, "Origin authentication is not configured");
    }

    const headers = new Headers(request.headers);
    for (const name of HOP_BY_HOP) headers.delete(name);
    headers.set("CF-Access-Client-Id", env.ACCESS_CLIENT_ID);
    headers.set("CF-Access-Client-Secret", env.ACCESS_CLIENT_SECRET);
    headers.set("X-Forwarded-Host", url.host);
    headers.set("X-Forwarded-Proto", "https");
    headers.set("X-Aixodia-Instance", instance);

    // The instance segment is this gateway's, not the engine's: the engine sees
    // the path it serves. "/" for a bare instance address.
    const forwarded = "/" + segments.slice(1).join("/");
    const target = new URL(forwarded + url.search, "https://" + origin);

    const upstream = new Request(target, {
      method: request.method,
      headers,
      body: request.method === "GET" || request.method === "HEAD" ? undefined : await request.arrayBuffer(),
      redirect: "manual"
    });

    try {
      const response = await fetch(upstream);
      const responseHeaders = new Headers(response.headers);
      for (const name of HOP_BY_HOP) responseHeaders.delete(name);
      // Every engine answer is per-instance live state; nothing here may be cached.
      responseHeaders.set("cache-control", "no-store");
      // A WebSocket upgrade cannot be proxied by a Worker; say so plainly rather
      // than letting the client see a truncated 101.
      return new Response(response.body, {
        status: response.status,
        statusText: response.statusText,
        headers: responseHeaders
      });
    } catch {
      return error(502, "Instance tunnel is unavailable");
    }
  }
};
export { INSTANCE_NAME, ORIGIN_HOST };
