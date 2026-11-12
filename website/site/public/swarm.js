// The live run of the Swarm page (#418 (technique pages, part 2)): eight
// seeds, each a row of the fault families it drew with ChaosMode::Swarm.
// The families come from the run (`Run.families`, the simulator's
// chaos_families event). A click on a row opens its run in the player.
import { mount } from "./player.js";

const runs = JSON.parse(document.getElementById("runs").textContent);
const names = JSON.parse(document.getElementById("swarm-families").textContent);
const table = document.getElementById("swarm-table");
const out = document.getElementById("swarm-out");
const root = document.getElementById("run");
const player = mount(root, runs[0], { label: "replicated register · swarm" });
let current = 0;

function el(tag, text, cls) {
  const e = document.createElement(tag);
  if (text !== undefined) e.textContent = text;
  if (cls) e.className = cls;
  return e;
}

function dots(list, on, kind) {
  return list.map((f) => {
    const td = el("td", on.includes(f) ? "●" : "·", "fam " + (on.includes(f) ? "on" : "off"));
    td.title = kind + " " + f + (on.includes(f) ? ": on" : ": off");
    return td;
  });
}

function draw() {
  const head = el("tr");
  head.append(el("th", "seed"), ...names.network.map((f, i) => {
    const th = el("th", String(i + 1), "fam");
    th.title = "network: " + f;
    return th;
  }), el("th", "", "gap"), ...names.storage.map((f, i) => {
    const th = el("th", String.fromCharCode(97 + i), "fam");
    th.title = "storage: " + f;
    return th;
  }), el("th", "faults"), el("th", "run"));
  const rows = runs.map((r, k) => {
    const tr = el("tr");
    tr.tabIndex = 0;
    tr.className = "pick" + (k === current ? " current" : "");
    tr.setAttribute("aria-label", "Open seed " + r.config.seed + " in the player");
    const f = r.families || { network: [], storage: [] };
    tr.append(
      el("td", String(r.config.seed)),
      ...dots(names.network, f.network, "network"),
      el("td", "", "gap"),
      ...dots(names.storage, f.storage, "storage"),
      el("td", String(r.faults.length)),
      el("td", r.report.passed ? "✓ passed" : "✗ failed", r.report.passed ? "on" : "bad")
    );
    const open = () => {
      current = k;
      player.show(r, { source: "recorded" });
      draw();
    };
    tr.addEventListener("click", open);
    tr.addEventListener("keydown", (e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), open()));
    return tr;
  });
  const all = (kind) => (f) => runs.some((r) => r.families && r.families[kind].includes(f));
  const cover = el("tr", undefined, "total");
  cover.append(
    el("td", "any"),
    ...names.network.map((f) => el("td", all("network")(f) ? "●" : "·", "fam " + (all("network")(f) ? "on" : "off"))),
    el("td", "", "gap"),
    ...names.storage.map((f) => el("td", all("storage")(f) ? "●" : "·", "fam " + (all("storage")(f) ? "on" : "off"))),
    el("td", ""),
    el("td", "")
  );
  table.replaceChildren(head, ...rows, cover);
  const full = runs.filter((r) => r.families && r.families.network.length === names.network.length && r.families.storage.length === names.storage.length).length;
  const covered = names.network.every(all("network")) && names.storage.every(all("storage"));
  out.className = "run-out " + (full === 0 && covered ? "same" : "diff");
  out.textContent =
    (full === 0 ? "No seed has every family on. " : full + " seed(s) have every family on. ") +
    (covered ? "Together, the eight seeds turn on each of the " + (names.network.length + names.storage.length) + " families." : "Some family is on in no seed.");
}

root.addEventListener("run", (e) => {
  const r = e.detail;
  if (r.config.seed === runs[current].config.seed) runs[current] = r;
  draw();
});
draw();
