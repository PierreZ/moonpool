// The reading of a run by the player (public/run-model.js), on the recorded
// runs that the Rust test writes (#416 (engine and player)).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import * as model from "../../public/run-model.js";

const load = (name) => JSON.parse(readFileSync(new URL("../runs/" + name + ".json", import.meta.url), "utf8"));
const [home] = load("home");
const [a, b] = load("determinism");

test("the workload lane is on top, then each process", () => {
  const l = model.lanes(home);
  assert.equal(l[0].role, "workload");
  assert.deepEqual(l.slice(1).map((x) => x.ip), ["10.0.1.1", "10.0.1.2", "10.0.1.3"]);
});

test("each message has the step where it left, and arrives after it", () => {
  for (const m of model.messages(home)) {
    assert.ok(m.sendAt >= 0, "message " + m.id + " has a msg_send event");
    assert.equal(home.events[m.sendAt].name, "msg_send");
    if (m.received_ms !== null) assert.ok(m.recvAt > m.sendAt);
  }
});

test("a down span starts at a stop and ends at the next boot of the same process", () => {
  const run = {
    events: [
      { seq: 0, time_ms: 1, source: "10.0.1.1", name: "server_boot", fields: {} },
      { seq: 1, time_ms: 5, source: "sim", name: "sim_fault", fields: { kind: "process_force_kill", ip: "10.0.1.1" } },
      { seq: 2, time_ms: 9, source: "10.0.1.2", name: "server_stop", fields: {} },
      { seq: 3, time_ms: 20, source: "10.0.1.1", name: "server_boot", fields: {} },
    ],
  };
  assert.deepEqual(model.downSpans(run), [
    { ip: "10.0.1.1", start: 1, startMs: 5, end: 3, endMs: 20 },
    { ip: "10.0.1.2", start: 2, startMs: 9, end: -1, endMs: null },
  ]);
});

test("faults name their lane or their link, with a glyph per family", () => {
  assert.deepEqual(model.faultTarget({ fields: { from: "a", to: "b" } }), { link: ["a", "b"] });
  assert.deepEqual(model.faultTarget({ fields: { ip: "a" } }), { ip: "a" });
  assert.equal(model.faultGlyph("process_force_kill"), "▼");
  assert.equal(model.faultGlyph("storage_crash"), "▣");
  assert.equal(model.faultGlyph("partition_created"), "⌁");
});

test("one seed matches itself one by one, and another seed differs", () => {
  assert.equal(model.firstDifference(a, a), -1);
  assert.equal(model.firstDifference(home, a), -1, "home and determinism record the same seed 256");
  const d = model.firstDifference(a, b);
  assert.ok(d >= 0);
  assert.notDeepEqual(a.events[d], b.events[d]);
  for (let i = 0; i < d; i++) assert.deepEqual(a.events[i], b.events[i]);
});

test("a passing run has no violation mark", () => {
  assert.equal(model.violationAt(home), -1);
  const broken = { ...home, report: { ...home.report, first_violation: { seq: home.events[5].seq } } };
  assert.equal(model.violationAt(broken), 5);
});

test("an event is one line of text, and the figures come from the run", () => {
  const fault = { time_ms: 506, source: "sim", name: "sim_fault", fields: { kind: "partition_created", from: "x", to: "y" } };
  assert.equal(model.eventText(fault), "  506 ms  sim       partition_created  from=x to=y");
  const f = model.figures(home);
  assert.equal(f.events, home.events.length);
  assert.equal(f.faults, home.faults.length);
  assert.match(f.print, /^[0-9a-f]{8}\/\d+$/);
});

test("the address gives a seed only when it is a whole number", () => {
  assert.equal(model.seedFrom("?seed=256"), 256);
  assert.equal(model.seedFrom("?seed=0"), 0);
  assert.equal(model.seedFrom("?seed=-1"), null);
  assert.equal(model.seedFrom("?seed=2.5"), null);
  assert.equal(model.seedFrom(""), null);
});
