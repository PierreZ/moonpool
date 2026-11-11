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
