import WebSocket from "ws";

export const BASE = process.env.RELAY_URL ?? "https://aixodia-relay.aixodia.workers.dev";
export const TOKEN = process.env.RELAY_TOKEN ?? "test-token";
export const WS_BASE = BASE.replace(/^http/, "ws");

const RUN = Math.random().toString(36).slice(2, 8);

export const timed = (p, ms, label) =>
  Promise.race([p, new Promise((_, rej) => setTimeout(() => rej(new Error(`timeout: ${label}`)), ms))]);

function openOnce(name, token) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`${WS_BASE}/room/${name}-${RUN}`, {
      headers: { Authorization: `Bearer ${token}` },
      family: 4,
    });
    const received = [];
    const raw = [];
    ws.on("message", (d) => { received.push(d.toString()); raw.push(d); });
    const closed = new Promise((r) => ws.on("close", (code, reason) => r({ code, reason: String(reason) })));
    const waitFor = (sub, ms = 5000) => new Promise((res, rej) => {
      const t0 = Date.now();
      const tick = () => {
        const hit = received.find((m) => (typeof sub === "string" ? m === sub : sub.test(m)));
        if (hit !== undefined) return res(hit);
        if (Date.now() - t0 > ms) return rej(new Error(`timeout waiting ${sub}; got ${JSON.stringify(received)}`));
        setTimeout(tick, 25);
      };
      tick();
    });
    const send = (x) => new Promise((res, rej) => ws.send(x, (e) => (e ? rej(e) : res())));
    ws.on("open", () => resolve({
      ws, received, raw, closed, waitFor, send,
      ready: Promise.resolve(),
      engine: (started, version = "t") => send(JSON.stringify({ role: "engine", started, version })),
      phone: () => send(JSON.stringify({ role: "phone" })),
    }));
    ws.on("error", reject);
  });
}

export async function dial(name, token = TOKEN) {
  let last;
  for (let i = 0; i < 3; i++) {
    try {
      return await openOnce(name, token);
    } catch (e) {
      last = e;
      await new Promise((r) => setTimeout(r, 400));
    }
  }
  throw last;
}
