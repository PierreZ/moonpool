// The Multiverse page (#418 (technique pages, part 2)): the tree of an
// exploration, recorded natively by crates/moonpool-wasm-demo/tests/
// exploration_tree.rs (the explorer forks, so it does not run in a browser).
// Each column is a timeline in the order it ran, each row a depth. A step
// shows the next timeline. The player shows the root, or the timeline that
// found the bug, which the engine can replay in the browser from its recipe.
import { mount, reduceMotion } from "./player.js";
import * as model from "./run-model.js";

const tree = JSON.parse(document.getElementById("tree").textContent);
const svg = document.getElementById("tree-svg");
const info = document.getElementById("tree-info");
const stepBtn = document.getElementById("tree-step");
const allBtn = document.getElementById("tree-all");
const bugBtn = document.getElementById("tree-bug");
const rootBtn = document.getElementById("tree-root");
const player = mount(document.getElementById("run"), tree.root, { label: "replicated register · bug on · root timeline", address: false });
const NS = "http://www.w3.org/2000/svg";
const rows = model.treeRows(tree.nodes);
const depth = Math.max(...rows.map((r) => r.depth));
const bugAt = tree.nodes.findIndex((n) => n.failed);
const W = 22;
const H = 34;
let shown = reduceMotion.matches ? tree.nodes.length : 1;
let selected = 0;

function node(tag, attrs, text) {
  const e = document.createElementNS(NS, tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
  if (text !== undefined) e.textContent = text;
  return e;
}

const x = (i) => 16 + i * W;
const y = (d) => 18 + d * H;

function draw() {
  svg.setAttribute("viewBox", "0 0 " + (32 + tree.nodes.length * W) + " " + (36 + depth * H));
  svg.setAttribute("width", String(32 + tree.nodes.length * W));
  svg.setAttribute("height", String(36 + depth * H));
  const parts = [];
  for (let d = 0; d <= depth; d++) parts.push(node("line", { x1: 0, x2: 32 + tree.nodes.length * W, y1: y(d), y2: y(d), class: "tree-rule" }));
  for (const r of rows) {
    if (r.index >= shown || r.parent === null) continue;
    parts.push(node("path", { d: `M${x(r.parent)},${y(r.depth - 1)} L${x(r.index)},${y(r.depth)}`, class: "tree-edge" }));
  }
  for (const r of rows) {
    if (r.index >= shown) continue;
    const cls = r.failed ? "bug" : r.discoveries.length ? "found" : "barren";
    const g = node("g", { class: "tree-node " + cls + (r.index === selected ? " sel" : ""), tabindex: 0, role: "button" });
    g.setAttribute("aria-label", "Timeline " + r.index);
    g.append(node("circle", { cx: x(r.index), cy: y(r.depth), r: 7 }));
    g.append(node("text", { x: x(r.index), y: y(r.depth) + 4, "text-anchor": "middle" }, r.failed ? "✗" : r.discoveries.length ? "+" : ""));
    const pick = () => {
      selected = r.index;
      draw();
    };
    g.addEventListener("click", pick);
    g.addEventListener("keydown", (e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), pick()));
    parts.push(g);
  }
  svg.replaceChildren(...parts);
  describe(tree.nodes[selected], selected);
  stepBtn.disabled = shown >= tree.nodes.length;
}

function describe(n, i) {
  const lines = [];
  lines.push(
    i === 0
      ? "Timeline 0 is the root: seed " + tree.seed + ", with no fork."
      : "Timeline " + i + " forks from timeline " + n.parent + " at draw " + n.fork_at + ": the same run up to that draw, then new random numbers."
  );
  if (i > 0) lines.push("Recipe: " + n.recipe + " (draw@seed, one pair per fork).");
  if (n.failed) lines.push("✗ It failed: the invariant broke. This is the bug.");
  const named = n.discoveries.filter((d) => d.assertion).map((d) => d.assertion);
  const other = n.discoveries.length - named.length;
  if (n.discoveries.length) {
    lines.push("+ " + n.discoveries.length + " new discoveries" + (other ? " (" + other + " of them new buckets)" : "") + ". A productive timeline gets " + tree.exploration.branching_factor + " children.");
  } else if (!n.failed) {
    lines.push("Nothing new: this branch ends here.");
  }
  const list = document.createElement("ul");
  for (const a of named.slice(0, 8)) {
    const li = document.createElement("li");
    li.textContent = a;
    list.append(li);
  }
  if (named.length > 8) {
    const li = document.createElement("li");
    li.textContent = "and " + (named.length - 8) + " more";
    list.append(li);
  }
  info.replaceChildren(...lines.map((t) => Object.assign(document.createElement("p"), { textContent: t })), ...(named.length ? [list] : []));
}

stepBtn.addEventListener("click", () => {
  shown = Math.min(tree.nodes.length, shown + 1);
  selected = shown - 1;
  draw();
});
allBtn.addEventListener("click", () => {
  shown = tree.nodes.length;
  draw();
});
bugBtn.addEventListener("click", () => {
  shown = Math.max(shown, bugAt + 1);
  selected = bugAt;
  draw();
  player.show(tree.bug, { source: "recorded" });
  player.runLive(tree.bug.config);
});
rootBtn.addEventListener("click", () => {
  selected = 0;
  draw();
  player.show(tree.root, { source: "recorded" });
});
draw();
