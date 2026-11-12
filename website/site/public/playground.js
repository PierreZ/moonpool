// The playground (#419 (playground)): a config from the controls, run in the
// engine (wasm in a Web Worker), drawn by the player. "Hunt" runs seeds one
// after the other and stops at the first red one; the page stays live, since
// each seed runs in the worker, and "Stop" ends the hunt between two seeds.
// The address holds the whole config, so a shared link gives the same run.
import { mount, reduceMotion } from "./player.js";
import * as engine from "./engine.js";
import * as model from "./run-model.js";

const [recorded] = JSON.parse(document.getElementById("runs").textContent);
const DEFAULTS = { ...recorded.config, network_faults: null, storage_faults: null, recipe: null };
const $ = (id) => document.getElementById(id);
const form = $("play");
const seedInput = $("play-seed");
const out = $("play-out");
const huntBtn = $("play-hunt");
const SWITCHES = { "play-net": ["chaos", "network"], "play-disk": ["chaos", "storage"], "play-reboots": ["chaos", "reboots"], "play-wipes": ["wipes"], "play-swarm": ["swarm"], "play-bug": ["bug"] };
const MAX_HUNT = 2000;
let hunting = false;

const player = mount($("run"), recorded, { label: "replicated register", address: false, seedField: false, autoplay: false });

function read() {
  const seed = model.seedFrom("?seed=" + seedInput.value);
  const c = structuredClone(DEFAULTS);
  c.seed = seed === null ? DEFAULTS.seed : seed;
  c.servers = Number($("play-servers").value);
  for (const [id, path] of Object.entries(SWITCHES)) {
    if (path.length === 2) c[path[0]][path[1]] = $(id).checked;
    else c[path[0]] = $(id).checked;
  }
  return c;
}

function write(c) {
  seedInput.value = String(c.seed);
  $("play-servers").value = String(c.servers);
  for (const [id, path] of Object.entries(SWITCHES)) $(id).checked = path.length === 2 ? c[path[0]][path[1]] : c[path[0]];
  $("play-wipes").disabled = !c.chaos.reboots;
}

function address(c) {
  history.replaceState(null, "", "?" + model.playQuery(c));
}

function cell(tag, text, cls) {
  const e = document.createElement(tag);
  e.textContent = text;
  if (cls) e.className = cls;
  return e;
}

function table(el, head, rows) {
  const tr = document.createElement("tr");
  tr.append(...head.map((h) => cell("th", h)));
  el.replaceChildren(tr, ...rows);
}

function report(run) {
  const v = run.report.first_violation;
  const verdict = $("report-verdict");
  verdict.className = "run-out " + (run.report.passed ? "same" : "bad");
  verdict.textContent = run.report.passed
    ? "✓ Seed " + run.config.seed + " passed: the invariant held at each step."
    : "✗ Seed " + run.config.seed + " broke the invariant at " + v.time_ms + " ms (event " + model.violationAt(run) + "): " + v.message + ".";
  $("report-time").textContent = (run.report.sim_time_ms / 1000).toFixed(2) + " s simulated";
  table($("report-assertions"), ["assertion", "kind", "pass", "fail"], model.assertionRows(run).map((a) => {
    const tr = document.createElement("tr");
    tr.append(cell("td", a.msg, a.failed ? "bad" : ""), cell("td", a.kind), cell("td", String(a.passes)), cell("td", String(a.fails), a.failed ? "bad" : ""));
    return tr;
  }));
  const counts = Object.entries(model.faultCounts(run)).sort((a, b) => b[1] - a[1]);
  const rows = counts.map(([k, n]) => {
    const tr = document.createElement("tr");
    tr.append(cell("td", k), cell("td", String(n)));
    return tr;
  });
  if (run.families) {
    for (const [kind, list] of [["network families", run.families.network], ["storage families", run.families.storage]]) {
      const tr = document.createElement("tr");
      tr.append(cell("td", kind), cell("td", list.length ? list.join(", ") : "none"));
      rows.push(tr);
    }
  }
  if (!rows.length) rows.push(Object.assign(document.createElement("tr"), { innerHTML: "<td>no fault in this run</td><td></td>" }));
  table($("report-faults"), ["fault the seed drew", "count"], rows);
}

/** Show a run; a red run puts the player at the first wrong event. */
function present(run) {
  report(run);
  const broken = model.violationAt(run);
  if (broken >= 0) player.seek(broken);
  else if (!reduceMotion.matches) {
    player.restart();
    player.play();
  }
}

async function runOne(c) {
  address(c);
  out.className = "run-out";
  out.textContent = "";
  const run = await player.runLive(c);
  if (run) present(run);
  return run;
}

async function hunt() {
  if (hunting) {
    hunting = false;
    return;
  }
  hunting = true;
  huntBtn.textContent = "■ Stop";
  const c = read();
  const t0 = performance.now();
  let done = 0;
  let found = null;
  try {
    while (hunting && done < MAX_HUNT) {
      out.className = "run-out";
      out.textContent = "Hunting: seed " + c.seed + ", " + done + (done === 1 ? " seed" : " seeds") + " done.";
      const { run, ms } = await engine.runConfig(c);
      done += 1;
      if (!run.report.passed) {
        found = { run, ms };
        break;
      }
      c.seed += 1;
    }
  } catch (e) {
    out.className = "run-out bad";
    out.textContent = "The engine did not run: " + e.message + ".";
    hunting = false;
    huntBtn.textContent = "Hunt";
    return;
  }
  const secs = ((performance.now() - t0) / 1000).toFixed(1);
  write(c);
  address(c);
  if (found) {
    player.show(found.run, { source: "live", ms: found.ms });
    present(found.run);
    out.className = "run-out bad";
    out.textContent = "✗ Seed " + c.seed + " is red, after " + done + (done === 1 ? " seed" : " seeds") + " in " + secs + " s. The player is at the first wrong event. Replay it: the print is the same.";
  } else {
    out.className = "run-out";
    out.textContent = hunting
      ? "No red seed in " + done + " seeds. " + (c.bug ? "Try other faults." : "The planted bug is off: switch it on.")
      : "Stopped after " + done + (done === 1 ? " seed" : " seeds") + ", all green. The next seed is " + c.seed + ".";
  }
  hunting = false;
  huntBtn.textContent = "Hunt";
}

form.addEventListener("submit", (e) => {
  e.preventDefault();
  if (!hunting) runOne(read());
});
form.addEventListener("change", (e) => {
  if (e.target.id === "play-reboots") $("play-wipes").disabled = !e.target.checked;
});
$("play-next").addEventListener("click", () => {
  const c = read();
  c.seed += 1;
  write(c);
  if (!hunting) runOne(c);
});
$("play-random").addEventListener("click", () => {
  const c = read();
  c.seed = Math.floor(Math.random() * 1e9);
  write(c);
  if (!hunting) runOne(c);
});
huntBtn.addEventListener("click", hunt);
$("play-replay").addEventListener("click", () => {
  if (!hunting) runOne(read());
});

const start = model.playConfig(location.search, DEFAULTS);
write(start);
report(recorded);
engine.warm();
runOne(start);
