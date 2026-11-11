// The live run of the Reboots page (#417 (technique pages, part 1)): the
// player, and under it each reboot of a server: its kind, when it went down,
// when it booted again, the version it read from its disk and the version
// the workload had committed then. All of it comes from the run's events
// (public/run-model.js reboots).
import { mount } from "./player.js";
import * as model from "./run-model.js";

const KIND = { graceful: "graceful", crash: "crash", wipe: "crash + wipe" };
const [recorded] = JSON.parse(document.getElementById("runs").textContent);
const root = document.getElementById("run");
const table = document.getElementById("reboot-table");
const out = document.getElementById("reboot-out");
const player = mount(root, recorded, { label: "replicated register · reboots and wipes" });

function cell(text, cls) {
  const td = document.createElement("td");
  td.textContent = text;
  if (cls) td.className = cls;
  return td;
}

function draw(cursor) {
  const run = player.run;
  const head = document.createElement("tr");
  head.append(...["server", "reboot", "down at", "back at", "version on its disk", "version committed"].map((t) => {
    const th = document.createElement("th");
    th.textContent = t;
    return th;
  }));
  const rows = model.reboots(run).map((r) => {
    const tr = document.createElement("tr");
    const seen = r.index <= cursor;
    const back = r.backIndex >= 0 && r.backIndex <= cursor;
    if (!seen) tr.className = "later";
    tr.append(
      cell(r.ip),
      cell((r.kind === "graceful" ? "▽ " : "▼ ") + KIND[r.kind], seen ? (r.kind === "graceful" ? "" : "hit") : ""),
      cell(r.at + " ms"),
      cell(r.back === null ? "not in this run" : back ? r.back + " ms" : seen ? "✗ down" : r.back + " ms", seen && !back ? "bad" : ""),
      cell(r.back === null ? "" : String(r.version), back ? "on" : ""),
      cell(r.back === null ? "" : String(model.committedAt(run, r.backIndex)))
    );
    return tr;
  });
  table.replaceChildren(head, ...rows);
  const servers = model.lanes(run).filter((l) => l.role !== "workload").length;
  const most = model.mostDown(run);
  out.className = "run-out " + (run.report.passed ? "same" : "bad");
  out.textContent = "Most servers down at the same time: " + most + " of " + servers + ". " +
    (run.report.passed ? "✓ The invariant held: each read returned the last committed write." : "✗ The invariant broke.");
}

player.onCursor(draw);
draw(player.cursor);
