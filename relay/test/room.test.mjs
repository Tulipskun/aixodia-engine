import { test } from "node:test";
import assert from "node:assert/strict";
import { BASE, dial, timed } from "./helpers.mjs";

test("401 without a token", async () => {
  const res = await fetch(`${BASE}/room/t2a`);
  assert.equal(res.status, 401);
});

test("401 with a wrong token", async () => {
  const res = await fetch(`${BASE}/room/t2a`, { headers: { Authorization: "Bearer nope" } });
  assert.equal(res.status, 401);
});

test("forwards frames both ways between engine and phone", async () => {
  const eng = await dial("t2fwd");
  await eng.engine(100);

  const ph = await dial("t2fwd");
  await ph.phone();

  await ph.send("hello-engine");
  assert.equal(await eng.waitFor("hello-engine"), "hello-engine");

  await eng.send("hello-phone");
  assert.equal(await ph.waitFor("hello-phone"), "hello-phone");

  eng.ws.close();
  ph.ws.close();
});

test("newer engine takes the slot, older engine is closed", async () => {
  const a = await dial("t3ho");
  await a.engine(100, "a");

  const b = await dial("t3ho");
  await b.engine(101, "b");

  assert.match(await a.waitFor(/superseded/, 8000), /superseded/);
  const ca = await timed(a.closed, 8000, "the old engine must be closed");
  assert.notEqual(ca.code, 1006);

  const c = await dial("t3ho");
  await c.engine(101, "c");
  assert.match(await b.waitFor(/superseded/, 8000), /superseded/);
  await timed(b.closed, 8000, "the superseded engine must be closed");

  c.ws.close();
});

test("a reconnect with the same started replaces the live engine", async () => {
  const a = await dial("t3same");
  await a.engine(700, "a");

  const again = await dial("t3same");
  await again.engine(700, "a");

  assert.match(await a.waitFor(/superseded/, 8000), /superseded/);
  await timed(a.closed, 8000, "the stale same-started socket must be closed");

  const ph = await dial("t3same");
  await ph.phone();
  await ph.waitFor(/"ctl":"presence"/, 8000);
  await again.send("still-engine");
  assert.equal(await ph.waitFor("still-engine"), "still-engine");

  again.ws.close();
  ph.ws.close();
});

test("an engine with an older started never takes the slot", async () => {
  const a = await dial("t3old");
  await a.engine(500, "a");

  const b = await dial("t3old");
  await b.engine(400, "b");
  assert.match(await b.waitFor(/superseded/, 8000), /superseded/);
  await timed(b.closed, 8000, "the older engine must be closed");

  const still = await Promise.race([
    a.closed.then(() => "closed"),
    new Promise((r) => setTimeout(() => r("open"), 1500)),
  ]);
  assert.equal(still, "open");

  a.ws.close();
});

test("the second phone closes the first, and the engine survives", async () => {
  const eng = await dial("t4sp");
  await eng.engine(1);

  const p1 = await dial("t4sp");
  await p1.phone();

  const p2 = await dial("t4sp");
  await p2.phone();

  assert.match(await p1.waitFor(/superseded-phone/, 8000), /superseded-phone/);
  await timed(p1.closed, 8000, "the first phone must be closed");
  await p2.waitFor(/"ctl":"presence"/, 8000);

  await eng.send("still-here");
  assert.equal(await p2.waitFor("still-here"), "still-here");

  eng.ws.close();
  p2.ws.close();
});

test("the phone is told when the engine attaches and when it leaves", async () => {
  const p = await dial("t4pres");
  await p.phone();

  const eng = await dial("t4pres");
  await eng.engine(1);
  assert.match(await p.waitFor(/"engine":true/, 8000), /"engine":true/);

  eng.ws.close();
  assert.match(await p.waitFor(/"engine":false/, 8000), /"engine":false/);

  p.ws.close();
});

test("a replacement engine makes presence fall before it rises again", async () => {
  const room = "t4hand";
  const ph = await dial(room);
  await ph.phone();

  const a = await dial(room);
  await a.engine(900, "a");
  await ph.waitFor(/"engine":true/, 8000);

  // Forget the first rise: the phone must see a new false -> true pair, or an
  // app with an open terminal keeps its dead streams after a handover.
  ph.received.length = 0;

  const b = await dial(room);
  await b.engine(901, "b");

  assert.match(await ph.waitFor(/"engine":false/, 8000), /"engine":false/);
  assert.match(await ph.waitFor(/"engine":true/, 8000), /"engine":true/);

  await ph.send("after-handover");
  assert.equal(await b.waitFor("after-handover"), "after-handover");

  b.ws.close();
  ph.ws.close();
});

const waitRaw = (c, n, ms = 8000) => timed(new Promise((res) => {
  const t0 = Date.now();
  const tick = () => {
    const hit = c.raw.find((b) => b.length === n);
    if (hit) return res(hit);
    if (Date.now() - t0 > ms) return res(undefined);
    setTimeout(tick, 25);
  };
  tick();
}), ms + 1000, "binary frame");

test("relays a 32 KiB binary frame in both directions", async () => {
  const eng = await dial("t5sz");
  await eng.engine(1);
  const ph = await dial("t5sz");
  await ph.phone();
  await ph.waitFor(/"ctl":"presence"/, 8000);

  const up = Buffer.alloc(32 * 1024, 7);
  await eng.send(up);
  const gotUp = await waitRaw(ph, up.length);
  assert.ok(gotUp, "engine->phone binary frame");
  assert.equal(gotUp.length, up.length);
  assert.equal(gotUp[0], 7);

  const down = Buffer.alloc(32 * 1024, 9);
  await ph.send(down);
  const gotDown = await waitRaw(eng, down.length);
  assert.ok(gotDown, "phone->engine binary frame");
  assert.equal(gotDown[down.length - 1], 9);

  eng.ws.close();
  ph.ws.close();
});

test("survives a long idle gap (hibernation) and still routes", async () => {  const eng = await dial("t5hiber");
  await eng.engine(1);
  const ph = await dial("t5hiber");
  await ph.phone();
  await ph.waitFor(/"ctl":"presence"/, 8000);

  await ph.send("before-idle");
  assert.equal(await eng.waitFor("before-idle"), "before-idle");

  await new Promise((r) => setTimeout(r, 35000));

  await ph.send("after-idle");
  assert.equal(await eng.waitFor("after-idle", 10000), "after-idle");

  await eng.send("engine-after-idle");
  assert.equal(await ph.waitFor("engine-after-idle", 10000), "engine-after-idle");

  eng.ws.close();
  ph.ws.close();
});

test("a frame sent before the role hello is never forwarded", async () => {
  const eng = await dial("t5norole");
  await eng.engine(1);
  const ph = await dial("t5norole");
  await ph.phone();

  const ghost = await dial("t5norole");
  await ghost.send("no-role-here");
  await new Promise((r) => setTimeout(r, 800));
  assert.ok(!eng.received.includes("no-role-here"), `engine saw ${JSON.stringify(eng.received)}`);

  ghost.ws.close();
  eng.ws.close();
  ph.ws.close();
});

test("an unknown role hello is ignored and gets no traffic", async () => {
  const eng = await dial("t5badrole");
  await eng.engine(1);
  const ph = await dial("t5badrole");
  await ph.phone();

  const weird = await dial("t5badrole");
  await weird.send(JSON.stringify({ role: "router" }));
  await weird.send("after-bad-role");
  await new Promise((r) => setTimeout(r, 800));
  assert.equal(weird.received.length, 0, `stranger saw ${JSON.stringify(weird.received)}`);
  assert.ok(!eng.received.includes("after-bad-role"));

  weird.ws.close();
  eng.ws.close();
  ph.ws.close();
});
