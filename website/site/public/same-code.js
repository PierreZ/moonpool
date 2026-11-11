// The live run of the Same code page (#417 (technique pages, part 1)): the
// lines of the register's server, and what each one did in this run. A row
// counts the events its line emitted up to the cursor; a hint row turns
// amber when the simulator rebooted the process there. "Find a strike" runs
// the next seeds in the engine until the chosen hint strikes.
import { mount } from "./player.js";
import * as engine from "./engine.js";
import * as model from "./run-model.js";

const data = JSON.parse(document.getElementById("same-code-data").textContent);
const [recorded] = JSON.parse(document.getElementById("runs").textContent);
const root = document.getElementById("run");
const rows = [...document.querySelectorAll("#code-table tr[data-event]")];
const out = document.getElementById("strike-out");
const form = document.getElementById("strike-form");
const player = mount(root, recorded, { label: "replicated register" });

function count(run, cursor, row) {
  const name = row.dataset.event;
  const label = row.dataset.label;
  let n = 0;
  for (let i = 0; i <= cursor && i < run.events.length; i++) {
    const e = run.events[i];
    if (e.name !== name) continue;
    if (label && e.fields.label !== label) continue;
    if (row.dataset.role && !model.lanes(run).some((l) => l.ip === e.source && l.role === row.dataset.role)) continue;
    n++;
  }
  return n;
}

function draw(cursor) {
  const run = player.run;
  for (const row of rows) {
    const n = count(run, cursor, row);
    const td = row.querySelector(".sim");
    const hint = Boolean(row.dataset.label);
    td.className = "sim " + (n ? (hint ? "hit" : "on") : "later");
    td.textContent = hint
      ? n ? "▼ struck " + n + "×: the process rebooted here" : "no strike in this run yet"
      : n ? "✓ " + n + "× " + row.dataset.event : "not yet";
  }
}

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  const label = form.elements.label.value;
  const start = player.run.config.seed;
  for (let k = 1; k <= data.tries; k++) {
    const seed = start + k;
    out.className = "run-out";
    out.textContent = "Seed " + seed + "…";
    let r;
    try {
      r = await engine.runConfig({ ...recorded.config, seed });
    } catch (err) {
      out.className = "run-out bad";
      out.textContent = "The engine did not run: " + err.message + ".";
      return;
    }
    const strike = model.hintStrikes(r.run, data.struck).find((s) => s.label === label);
    if (strike) {
      player.show(r.run, { source: "live", ms: r.ms });
      player.dispatchRun(r.run);
      out.className = "run-out diff";
      out.textContent = "Seed " + seed + ": the simulator rebooted " + strike.ip + " at “" + label + "” at " + strike.time_ms + " ms. " + k + (k === 1 ? " seed" : " seeds") + " run.";
      return;
    }
  }
  out.className = "run-out";
  out.textContent = "No strike at this hint in the next " + data.tries + " seeds.";
});

player.onCursor(draw);
draw(player.cursor);
