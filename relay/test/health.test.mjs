import { test } from "node:test";
import assert from "node:assert/strict";
import { BASE } from "./helpers.mjs";

test("healthz", async () => {
  const res = await fetch(`${BASE}/healthz`);
  assert.equal(res.status, 200);
  assert.equal(await res.text(), "ok");
});
