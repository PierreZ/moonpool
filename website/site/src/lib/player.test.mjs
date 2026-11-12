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

const [storage] = load("storage");
const [reboot] = load("reboots");
const [same] = load("same-code");
const [net] = load("network");

test("the sectors of a log: written, then durable, then the fate a crash gave them", () => {
  const ev = (name, source, fields) => ({ seq: 0, time_ms: 0, source, name, fields });
  const run = {
    events: [
      ev("log_written", "a", { sector: 0 }),
      ev("log_synced", "a", { sector: 0 }),
      ev("log_written", "a", { sector: 1 }),
      ev("sim_fault", "sim", { kind: "storage_crash_sectors", ip: "a", lost: "1", kept_old: "", kept_new: "", latent: "", shorn: "", lost_synced: "" }),
      ev("sim_fault", "sim", { kind: "storage_wipe", ip: "a" }),
    ],
  };
  assert.deepEqual([...model.sectorStates(run, 0).get("a")], [[0, "written"]]);
  assert.deepEqual([...model.sectorStates(run, 2).get("a")], [[0, "durable"], [1, "written"]]);
  assert.deepEqual([...model.sectorStates(run, 3).get("a")], [[0, "durable"], [1, "lost"]]);
  assert.equal(model.sectorStates(run, 4).get("a").size, 0, "a wipe empties the disk");
});

test("the storage page's run loses an unsynced sector, and no synced one", () => {
  const end = model.sectorStates(storage, storage.events.length - 1);
  assert.ok([...end.values()].some((d) => [...d.values()].includes("lost")));
  assert.deepEqual(model.lostSynced(storage), []);
});

test("reboots name their kind, and the reboots page's run has a crash and a wipe", () => {
  const kinds = model.reboots(reboot).map((r) => r.kind);
  assert.ok(kinds.includes("crash"));
  assert.ok(kinds.includes("wipe"));
  const wiped = model.reboots(reboot).find((r) => r.kind === "wipe" && r.back !== null);
  assert.ok(wiped, "the wiped server boots again within the run");
  assert.equal(wiped.version, 0, "a wiped server boots on an empty disk");
  const crashed = model.reboots(reboot).find((r) => r.kind === "crash" && r.back !== null);
  assert.ok(crashed && crashed.version > 0, "a crashed server boots on what it synced");
  assert.ok(crashed.backIndex > crashed.index);
  assert.ok(model.mostDown(reboot) <= 1, "attrition keeps a quorum up");
});

test("the same-code page's run strikes at the hint before the sync", () => {
  const strikes = model.hintStrikes(same, "hint: rebooting the process at its own hint");
  assert.ok(strikes.some((s) => s.label === "register: record written, not synced"));
});

test("the network page's run has a partition, a clog and a black hole", () => {
  const c = model.faultCounts(net);
  for (const k of ["partition_created", "interface_clogged", "black_hole"]) assert.ok(c[k] > 0, k);
});

test("the committed version at an event is the newest write_committed so far", () => {
  const ev = (name, version) => ({ seq: 0, time_ms: 0, source: "c", name, fields: { version } });
  const run = { events: [ev("write_committed", 1), ev("write_applied", 9), ev("write_committed", 3)] };
  assert.equal(model.committedAt(run, -1), 0);
  assert.equal(model.committedAt(run, 1), 1);
  assert.equal(model.committedAt(run, 5), 3);
});

const [debug] = load("debugging");
const explored = load("exploration");
const swarm = load("swarm");
const cov = load("assertions");

test("the walk back goes from the broken read to the missed write and the rule", () => {
  const steps = model.walkBack(debug);
  assert.ok(steps.length >= 5, JSON.stringify(steps));
  assert.equal(steps[0].index, model.violationAt(debug));
  assert.match(steps[1].text, /kept one reply/);
  assert.match(steps[2].text, /served version \d+: it did not have version \d+/);
  assert.equal(debug.events[steps[3].index].name, "write_committed");
  assert.match(steps.at(-1).text, /quorum/);
  for (const s of steps) assert.ok(s.index >= 0 && s.index < debug.events.length);
  assert.deepEqual(model.walkBack(home), [], "a passing run has no walk back");
});

test("the exploration tree: the root first, each child under its parent, the bug a leaf", () => {
  const rows = model.treeRows(explored.nodes);
  assert.equal(rows.length, explored.nodes.length);
  assert.equal(rows[0].index, 0);
  assert.equal(rows[0].depth, 0);
  rows.forEach((r, k) => {
    if (r.parent === null) return;
    const p = rows.findIndex((x) => x.index === r.parent);
    assert.ok(p < k && rows[p].depth === r.depth - 1, "a child comes after its parent, one deeper");
  });
  assert.ok(explored.nodes.some((n) => n.failed));
  assert.equal(explored.root.report.passed, true);
  assert.equal(explored.bug.report.passed, false);
});

test("the exploration tree keeps each recipe as a string, so a 64-bit seed stays exact", () => {
  const text = readFileSync(new URL("../runs/exploration.json", import.meta.url), "utf8");
  for (const m of text.matchAll(/[:,[]\s*(\d{16,})/g)) assert.ok(Number.isSafeInteger(Number(m[1])), "a number above 2^53: " + m[1]);
  assert.match(explored.bug.config.recipe, /^\d+@\d+( -> \d+@\d+)*$/);
  const bug = explored.nodes.find((n) => n.failed);
  assert.equal(bug.recipe, explored.bug.config.recipe);
});

test("swarm: no seed has every family on, and the eight seeds cover each family", () => {
  const net = new Set();
  const sto = new Set();
  for (const r of swarm) {
    assert.ok(r.families.network.length < 9 || r.families.storage.length < 12);
    r.families.network.forEach((f) => net.add(f));
    r.families.storage.forEach((f) => sto.add(f));
  }
  assert.equal(net.size, 9);
  assert.equal(sto.size, 12);
});

test("coverage: a check no seed reaches stays missing, and the run is not stable", () => {
  assert.equal(cov.stable, false);
  assert.deepEqual(cov.missing, ["a server drops a damaged frame"]);
  assert.equal(cov.seeds.length, cov.config.max_seeds);
});

test("the playground config goes to the address and back", () => {
  const defaults = { scenario: "register", seed: 1, servers: 3, bug: false, wipes: false, swarm: false, chaos: { network: true, storage: false, reboots: true } };
  assert.deepEqual(model.playConfig("", defaults), defaults);
  const c = { ...defaults, seed: 42, servers: 5, bug: true, swarm: true, chaos: { network: false, storage: true, reboots: true } };
  assert.deepEqual(model.playConfig("?" + model.playQuery(c), defaults), c);
  const bad = model.playConfig("?seed=-3&servers=9&net=yes&bug=1", defaults);
  assert.equal(bad.seed, 1);
  assert.equal(bad.servers, 3);
  assert.equal(bad.chaos.network, true);
  assert.equal(bad.bug, true);
});

test("the report marks a broken always assertion, never a sometimes one", () => {
  const rows = model.assertionRows(debug);
  const broken = rows.filter((r) => r.failed).map((r) => r.msg);
  assert.deepEqual(broken, ["a read returns the last committed write"]);
  assert.ok(model.assertionRows(home).every((r) => !r.failed));
});
