import test from "node:test";
import assert from "node:assert/strict";
import worker, { INSTANCE_NAME, ORIGIN_HOST } from "../src/index.js";

test("validates instance names", () => {
  for (const value of ["main", "phone-2", "a", "x9"]) assert.equal(INSTANCE_NAME.test(value), true);
  for (const value of ["", "-main", "main-", "main--two", "../etc", "a/b", "Main", "a".repeat(33)]) {
    assert.equal(INSTANCE_NAME.test(value), false);
  }
});

test("restricts origin hostname suffix", () => {
  assert.equal(ORIGIN_HOST.test("phone-2.aixodia.dpdns.org"), true);
  assert.equal(ORIGIN_HOST.test("attacker.example"), false);
  assert.equal(ORIGIN_HOST.test("phone-2.example.org"), false);
});

test("fails closed without origin authentication", async () => {
  const response = await worker.fetch(new Request("https://engines.aixodia.dpdns.org/main/healthz"), {
    ENGINE_ORIGINS: { get: async () => "main.aixodia.dpdns.org" }
  });
  assert.equal(response.status, 503);
});

test("fails closed when the instance registry is missing", async () => {
  const response = await worker.fetch(new Request("https://engines.aixodia.dpdns.org/main/healthz"), {});
  assert.equal(response.status, 503);
});

test("strips the instance segment and forwards method, query, body and token", async () => {
  let seen;
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async request => {
    seen = request;
    return new Response("ok", { status: 200 });
  };
  try {
    const request = new Request("https://engines.aixodia.dpdns.org/phone-2/api/chat?stream=1", {
      method: "POST",
      headers: { "content-type": "application/json", connection: "keep-alive", host: "engines.aixodia.dpdns.org" },
      body: JSON.stringify({ messages: [] })
    });
    const response = await worker.fetch(request, {
      ENGINE_ORIGINS: { get: async key => (key === "phone-2" ? "phone-2.aixodia.dpdns.org" : null) },
      ACCESS_CLIENT_ID: "test-id",
      ACCESS_CLIENT_SECRET: "test-secret"
    });
    assert.equal(response.status, 200);
    assert.equal(await response.text(), "ok");
    assert.equal(seen.url, "https://phone-2.aixodia.dpdns.org/api/chat?stream=1");
    assert.equal(seen.headers.get("cf-access-client-id"), "test-id");
    assert.equal(seen.headers.get("cf-access-client-secret"), "test-secret");
    assert.equal(seen.headers.get("x-aixodia-instance"), "phone-2");
    assert.equal(seen.headers.get("connection"), null);
    assert.deepEqual(await seen.json(), { messages: [] });
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("a bare instance address becomes the root path", async () => {
  let seen;
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async request => {
    seen = request;
    return new Response("ok", { status: 200 });
  };
  try {
    await worker.fetch(new Request("https://engines.aixodia.dpdns.org/main"), {
      ENGINE_ORIGINS: { get: async () => "main.aixodia.dpdns.org" },
      ACCESS_CLIENT_ID: "id",
      ACCESS_CLIENT_SECRET: "secret"
    });
    assert.equal(seen.url, "https://main.aixodia.dpdns.org/");
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("returns 404 for unknown instances", async () => {
  const response = await worker.fetch(new Request("https://engines.aixodia.dpdns.org/nope/healthz"), {
    ENGINE_ORIGINS: { get: async () => null },
    ACCESS_CLIENT_ID: "id",
    ACCESS_CLIENT_SECRET: "secret"
  });
  assert.equal(response.status, 404);
});

test("never forwards an origin that is not this zone", async () => {
  const response = await worker.fetch(new Request("https://engines.aixodia.dpdns.org/main/healthz"), {
    ENGINE_ORIGINS: { get: async () => "evil.example.com" },
    ACCESS_CLIENT_ID: "id",
    ACCESS_CLIENT_SECRET: "secret"
  });
  assert.equal(response.status, 404);
});

test("reports an unreachable tunnel as 502", async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => {
    throw new Error("connection refused");
  };
  try {
    const response = await worker.fetch(new Request("https://engines.aixodia.dpdns.org/main/healthz"), {
      ENGINE_ORIGINS: { get: async () => "main.aixodia.dpdns.org" },
      ACCESS_CLIENT_ID: "id",
      ACCESS_CLIENT_SECRET: "secret"
    });
    assert.equal(response.status, 502);
  } finally {
    globalThis.fetch = originalFetch;
  }
});
