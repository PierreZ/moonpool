// What the player draws, read from a run of the site's engine (#416 (engine
// and player)). Each function here only reads the run: the messages, the
// faults, the dead spans and the checks come from its events. No simulation
// logic is in this file. `src/lib/player.test.mjs` tests it.
//
// The model of a run is `Run` in crates/moonpool-wasm-demo/src/engine.rs.

/** The lanes, top to bottom: the workload, then the processes. */
export function lanes(run) {
  const work = run.processes.filter((p) => p.role === "workload");
  const procs = run.processes.filter((p) => p.role !== "workload");
  return work.concat(procs).map((p) => ({ ip: p.ip, name: p.name, role: p.role }));
}

/**
 * Each message with the index of its `msg_send` and `msg_recv` events, so the
 * player shows it from the step where it left.
 */
export function messages(run) {
  const send = new Map();
  const recv = new Map();
  run.events.forEach((e, i) => {
    if (e.name === "msg_send" && !send.has(e.fields.msg)) send.set(e.fields.msg, i);
    if (e.name === "msg_recv" && !recv.has(e.fields.msg)) recv.set(e.fields.msg, i);
  });
  return run.messages.map((m) => ({
    ...m,
    sendAt: send.has(m.id) ? send.get(m.id) : -1,
    recvAt: recv.has(m.id) ? recv.get(m.id) : -1,
  }));
}

/** The lane (an ip) that a fault hits, or the two ends of a cut link. */
export function faultTarget(fault) {
  const f = fault.fields || {};
  if (f.from && f.to) return { link: [f.from, f.to] };
  if (f.ip) return { ip: f.ip };
  return {};
}

/** A short glyph per fault family. Colour is never the only sign. */
export function faultGlyph(kind) {
  if (kind.startsWith("process_")) return "▼";
  if (kind.startsWith("storage_") || kind.startsWith("disk_")) return "▣";
  return "⌁";
}

/**
 * The spans where a process is down: from its `server_stop` event or a
 * `process_force_kill` fault, to its next `server_boot`, or to the end.
 * `start` and `end` are event indexes; `end` is -1 when it never came back.
 */
export function downSpans(run) {
  const spans = [];
  const open = new Map();
  run.events.forEach((e, i) => {
    let ip = null;
    if (e.name === "server_stop") ip = e.source;
    else if (e.name === "sim_fault" && e.fields.kind === "process_force_kill") ip = e.fields.ip;
    if (ip && !open.has(ip)) {
      open.set(ip, { ip, start: i, startMs: e.time_ms, end: -1, endMs: null });
    } else if (e.name === "server_boot" && open.has(e.source)) {
      const span = open.get(e.source);
      span.end = i;
      span.endMs = e.time_ms;
      spans.push(span);
      open.delete(e.source);
    }
  });
  return spans.concat([...open.values()]);
}

/** The index of the event where the invariant broke first, or -1. */
export function violationAt(run) {
  const v = run.report.first_violation;
  if (!v) return -1;
  return run.events.findIndex((e) => e.seq === v.seq);
}

/**
 * The first index where two runs record a different event, or -1 when every
 * event matches one by one. A run that is shorter differs at its end.
 */
export function firstDifference(a, b) {
  const n = Math.max(a.events.length, b.events.length);
  for (let i = 0; i < n; i++) {
    const x = a.events[i];
    const y = b.events[i];
    if (!x || !y || JSON.stringify(x) !== JSON.stringify(y)) return i;
  }
  return -1;
}

/** One event as a line of text: time, source, name, fields. */
export function eventText(e) {
  const fields = Object.entries(e.fields)
    .filter(([k]) => !(e.name === "sim_fault" && k === "kind"))
    .map(([k, v]) => k + "=" + v)
    .join(" ");
  const name = e.name === "sim_fault" ? e.fields.kind : e.name;
  return String(e.time_ms).padStart(5) + " ms  " + e.source.padEnd(8) + "  " + name + (fields ? "  " + fields : "");
}

/** The key figures of a run. */
export function figures(run) {
  const delivered = run.messages.filter((m) => m.received_ms !== null).length;
  return {
    simMs: run.report.sim_time_ms,
    events: run.events.length,
    messages: delivered + "/" + run.messages.length,
    faults: run.faults.length,
    committed: run.events.filter((e) => e.name === "write_committed").length,
    print: run.fingerprint.events.slice(0, 8) + "/" + run.fingerprint.draws,
  };
}

/** A seed from the address (`?seed=256`), or null. */
export function seedFrom(search) {
  const p = new URLSearchParams(search);
  if (!p.has("seed")) return null;
  const s = p.get("seed").trim();
  return /^[0-9]{1,15}$/.test(s) ? Number(s) : null;
}

/** The fates of a crash, in the order of the storage page's legend. */
export const FATES = ["kept_old", "kept_new", "lost", "latent", "shorn", "lost_synced"];

/**
 * The sectors of each server's log after the event at `cursor`: `written`
 * after `log_written`, `durable` after `log_synced` (a `sync_all` makes every
 * written sector durable), then the fate a `storage_crash_sectors` fault
 * gave it, and nothing after a `storage_wipe`. A Map from ip to a Map from
 * sector to state.
 */
export function sectorStates(run, cursor) {
  const out = new Map();
  const disk = (ip) => {
    if (!out.has(ip)) out.set(ip, new Map());
    return out.get(ip);
  };
  for (let i = 0; i <= cursor && i < run.events.length; i++) {
    const e = run.events[i];
    const f = e.fields;
    if (e.name === "log_written") disk(e.source).set(f.sector, "written");
    else if (e.name === "log_synced") {
      const d = disk(e.source);
      for (const [s, state] of d) if (state === "written") d.set(s, "durable");
      d.set(f.sector, "durable");
    } else if (e.name === "sim_fault" && f.kind === "storage_wipe") disk(f.ip).clear();
    else if (e.name === "sim_fault" && f.kind === "storage_crash_sectors") {
      const d = disk(f.ip);
      for (const fate of FATES) {
        for (const s of String(f[fate] || "").split(",").filter(Boolean)) d.set(Number(s), fate);
      }
    }
  }
  return out;
}

/** The sectors that a sync reported durable and a crash lost (the oracle). */
export function lostSynced(run) {
  return run.faults
    .filter((f) => f.kind === "storage_crash_sectors" && f.fields.lost_synced)
    .map((f) => ({ ip: f.fields.ip, sectors: f.fields.lost_synced, time_ms: f.time_ms }));
}

/** How many faults of each kind a run recorded. */
export function faultCounts(run) {
  const out = {};
  for (const f of run.faults) out[f.kind] = (out[f.kind] || 0) + 1;
  return out;
}

/**
 * Each reboot of a server: when it went down, its kind (`graceful`, `crash`,
 * `wipe`), when it booted again (`back`, at event `backIndex`) and the
 * version it read from its disk.
 */
export function reboots(run) {
  const out = [];
  const open = new Map();
  run.events.forEach((e, i) => {
    const f = e.fields;
    if (e.name === "sim_fault" && f.kind === "process_graceful_shutdown" && !open.has(f.ip)) {
      open.set(f.ip, { ip: f.ip, kind: "graceful", at: e.time_ms, index: i, back: null, backIndex: -1, version: null });
    } else if (e.name === "sim_fault" && f.kind === "process_force_kill") {
      if (!open.has(f.ip)) open.set(f.ip, { ip: f.ip, kind: "crash", at: e.time_ms, index: i, back: null, backIndex: -1, version: null });
    } else if (e.name === "sim_fault" && f.kind === "storage_wipe" && open.has(f.ip)) {
      open.get(f.ip).kind = "wipe";
    } else if (e.name === "server_boot" && open.has(e.source)) {
      const r = open.get(e.source);
      r.back = e.time_ms;
      r.backIndex = i;
      r.version = f.version;
      out.push(r);
      open.delete(e.source);
    }
  });
  return out.concat([...open.values()]).sort((a, b) => a.index - b.index);
}

/** The most servers down at the same time, from the down spans. */
export function mostDown(run) {
  const edges = [];
  for (const s of downSpans(run)) {
    edges.push([s.start, 1]);
    edges.push([s.end >= 0 ? s.end : run.events.length, -1]);
  }
  edges.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  let now = 0;
  let most = 0;
  for (const [, d] of edges) {
    now += d;
    most = Math.max(most, now);
  }
  return most;
}

/** The `hint!` labels where the simulator rebooted a process, in order. */
export function hintStrikes(run, name) {
  return run.events.filter((e) => e.name === name).map((e) => ({ ip: e.source, label: e.fields.label, time_ms: e.time_ms }));
}

/** The newest version the workload saw committed at or before event `index`, or 0. */
export function committedAt(run, index) {
  let v = 0;
  for (let i = 0; i <= index && i < run.events.length; i++) {
    const e = run.events[i];
    if (e.name === "write_committed") v = Math.max(v, e.fields.version);
  }
  return v;
}

/**
 * The walk back from a broken read of the register to the cause, one step
 * per event: the read, the reply it kept, what that server served, the
 * commit it missed, the write that never reached it, and the rule the read
 * broke. Each step is `{ index, text }`. Empty when the run passed.
 */
export function walkBack(run) {
  const v = violationAt(run);
  if (v < 0) return [];
  const ev = run.events;
  const read = ev[v];
  const client = read.source;
  const seen = read.fields.version;
  const replies = read.fields.replies;
  const rounds = new Set(["write_committed", "write_failed", "read_returned", "read_failed"]);
  const roundStart = (end) => {
    let i = end;
    while (i > 0 && !(rounds.has(ev[i - 1].name) && ev[i - 1].source === client)) i--;
    return i;
  };
  let commit = -1;
  for (let i = 0; i < v; i++) {
    if (ev[i].name === "write_committed" && ev[i].fields.version > seen) commit = i;
  }
  const kept = [];
  for (let i = roundStart(v); i < v && kept.length < replies; i++) {
    const e = ev[i];
    if (e.name === "msg_recv" && e.source === client && e.fields.kind === "read_reply") kept.push(i);
  }
  const steps = [{ index: v, text: "The read returned version " + seen + "." }];
  const stale = kept.find((i) => {
    const s = servedFor(ev, ev[i]);
    return s >= 0 && ev[s].fields.version === seen;
  });
  if (stale === undefined || commit < 0) return steps;
  const server = ev[stale].fields.from;
  const served = servedFor(ev, ev[stale]);
  const version = ev[commit].fields.version;
  steps.push({
    index: stale,
    text: "The client kept " + (replies === 1 ? "one reply" : replies + " replies") + ", the first to come back: this one, from " + server + ".",
  });
  steps.push({ index: served, text: server + " served version " + seen + ": it did not have version " + version + "." });
  steps.push({ index: commit, text: "Version " + version + " was committed before the read, with " + ev[commit].fields.acks + " acks." });
  const sends = [];
  for (let i = roundStart(commit); i < commit; i++) {
    if (ev[i].name === "msg_send" && ev[i].source === client && ev[i].fields.kind === "write") sends.push(i);
  }
  const toServer = sends.find((i) => ev[i].fields.to === server);
  if (toServer === undefined) {
    const to = sends.map((i) => ev[i].fields.to).join(" and ");
    steps.push({
      index: sends.length ? sends[0] : commit,
      text: "The write of version " + version + " went to " + (to || "no server") + ". The call to " + server + " got no connection before its timeout, so " + server + " never had it.",
    });
  } else {
    const msg = run.messages.find((m) => m.id === ev[toServer].fields.msg);
    steps.push({
      index: toServer,
      text: msg && msg.received_ms === null
        ? "The write of version " + version + " to " + server + " was lost on the way."
        : "The write of version " + version + " to " + server + " arrived after the commit.",
    });
  }
  const quorum = Math.floor(lanesOf(run) / 2) + 1;
  steps.push({
    index: v,
    text: "A quorum of replies (" + quorum + " of " + lanesOf(run) + ") always holds one server that has version " + version +
      ". The read kept " + replies + ". That is the bug: a read must wait for a quorum.",
  });
  return steps;
}

/** The `read_served` event of the server that sent a reply, or -1. */
function servedFor(ev, recv) {
  const send = ev.findIndex((e) => e.name === "msg_send" && e.fields.msg === recv.fields.msg);
  for (let i = send - 1; i >= 0; i--) {
    if (ev[i].name === "read_served" && ev[i].source === ev[send].source) return i;
  }
  return -1;
}

function lanesOf(run) {
  return run.processes.filter((p) => p.role !== "workload").length;
}

/**
 * The tree of an exploration (src/runs/exploration.json) in depth-first
 * order: each node with its index, its depth and its parent. The children
 * of a node come in the order they ran.
 */
export function treeRows(nodes) {
  const children = new Map();
  nodes.forEach((n, i) => {
    const p = n.parent === null ? -1 : n.parent;
    if (!children.has(p)) children.set(p, []);
    children.get(p).push(i);
  });
  const out = [];
  const visit = (i, depth) => {
    out.push({ ...nodes[i], index: i, depth });
    for (const c of children.get(i) || []) visit(c, depth + 1);
  };
  for (const r of children.get(-1) || []) visit(r, 0);
  return out;
}

/**
 * The playground's config from the address (#419 (playground)): `seed`,
 * `servers` (1 to 5), and the switches `net`, `disk`, `reboots`, `wipes`,
 * `swarm` and `bug` as 1 or 0. A missing or bad value keeps the default.
 */
export function playConfig(search, defaults) {
  const p = new URLSearchParams(search);
  const flag = (name, value) => (p.get(name) === "1" ? true : p.get(name) === "0" ? false : value);
  const seed = seedFrom(search);
  const servers = Number(p.get("servers"));
  return {
    ...defaults,
    seed: seed === null ? defaults.seed : seed,
    servers: Number.isInteger(servers) && servers >= 1 && servers <= 5 ? servers : defaults.servers,
    chaos: {
      network: flag("net", defaults.chaos.network),
      storage: flag("disk", defaults.chaos.storage),
      reboots: flag("reboots", defaults.chaos.reboots),
    },
    wipes: flag("wipes", defaults.wipes),
    swarm: flag("swarm", defaults.swarm),
    bug: flag("bug", defaults.bug),
  };
}

/** The address query of a playground config: the inverse of `playConfig`. */
export function playQuery(config) {
  const b = (v) => (v ? "1" : "0");
  return new URLSearchParams({
    seed: String(config.seed),
    servers: String(config.servers),
    net: b(config.chaos.network),
    disk: b(config.chaos.storage),
    reboots: b(config.chaos.reboots),
    wipes: b(config.wipes),
    swarm: b(config.swarm),
    bug: b(config.bug),
  }).toString();
}

/**
 * The assertions of a run for the report, in the run's order: each with its
 * kind, its counts and whether it failed.
 */
export function assertionRows(run) {
  const must = (k) => k.endsWith("always") || k === "unreachable";
  return run.assertions.map((a) => ({ ...a, failed: a.fails > 0 && must(a.kind) }));
}
