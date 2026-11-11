// The live run of the Network page (#417 (technique pages, part 1)): the
// player, and a switch per network fault family. The same seed runs again
// with the families that stay on; the table counts the faults of each kind
// in the two runs. The names of the families come from NETWORK_FAULTS in
// crates/moonpool-wasm-demo/src/engine.rs.
import { mount } from "./player.js";
import * as engine from "./engine.js";
import * as model from "./run-model.js";

const [recorded] = JSON.parse(document.getElementById("runs").textContent);
const root = document.getElementById("run");
const form = document.getElementById("families");
const table = document.getElementById("fault-table");
const out = document.getElementById("families-out");
const player = mount(root, recorded, { label: "replicated register · network faults" });

// The run of the current seed with every family on: the reference.
let full = recorded;

function checked() {
  return [...form.querySelectorAll("input[name=family]")].filter((i) => i.checked).map((i) => i.value);
}

function cell(text, cls) {
  const td = document.createElement("td");
  td.textContent = text;
  if (cls) td.className = cls;
  return td;
}

function draw() {
  const run = player.run;
  const a = model.faultCounts(full);
  const b = model.faultCounts(run);
  const kinds = [...new Set([...Object.keys(a), ...Object.keys(b)])].sort();
  const same = run === full || run.config.network_faults === null;
  const head = document.createElement("tr");
  head.append(...["fault", "every family on", same ? "this run" : "your families"].map((t) => {
    const th = document.createElement("th");
    th.textContent = t;
    return th;
  }));
  const rows = kinds.map((k) => {
    const tr = document.createElement("tr");
    const x = a[k] || 0;
    const y = b[k] || 0;
    tr.append(cell(k), cell(String(x)), cell(String(y), y < x ? "later" : y > x ? "hit" : ""));
    return tr;
  });
  const fa = model.figures(full);
  const fb = model.figures(run);
  const sum = document.createElement("tr");
  sum.append(cell("messages delivered"), cell(fa.messages), cell(fb.messages));
  const com = document.createElement("tr");
  com.append(cell("writes committed"), cell(String(fa.committed)), cell(String(fb.committed)));
  table.replaceChildren(head, ...rows, sum, com);
  if (same) {
    out.className = "run-out";
    out.textContent = "Every family is on. Switch some off and run the same seed again.";
    return;
  }
  const d = model.firstDifference(full, run);
  const on = run.config.network_faults;
  out.className = "run-out " + (d < 0 ? "same" : "diff");
  out.textContent = d < 0
    ? "= The two runs are the same, event for event: this seed did not use the families you switched off."
    : "≠ With " + (on.length ? on.join(", ") : "no network family") + " on, the seed makes another run: the first event that differs is event " + (d + 1) + ".";
}

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  const seed = player.run.config.seed;
  const base = { ...recorded.config, seed };
  out.className = "run-out";
  out.textContent = "Running seed " + seed + " in your browser…";
  try {
    if (full.config.seed !== seed) full = (await engine.runConfig({ ...base, network_faults: null })).run;
  } catch (err) {
    out.className = "run-out bad";
    out.textContent = "The engine did not run: " + err.message + ".";
    return;
  }
  const families = checked();
  await player.runLive({ ...base, network_faults: families.length === form.querySelectorAll("input[name=family]").length ? null : families });
});

form.querySelector("#families-all").addEventListener("click", () => {
  for (const i of form.querySelectorAll("input[name=family]")) i.checked = true;
});
form.querySelector("#families-none").addEventListener("click", () => {
  for (const i of form.querySelectorAll("input[name=family]")) i.checked = false;
});

root.addEventListener("run", (e) => {
  const run = e.detail;
  if (run.config.network_faults === null) full = run;
  draw();
});
draw();
