// The debugging page (#418 (technique pages, part 2)): a seed of the register
// with the planted bug. The report names the failing seed, the engine replays
// that seed alone, and "Step back" walks from the broken invariant to the
// first wrong event, one step per click (public/run-model.js walkBack).
import { mount } from "./player.js";
import * as model from "./run-model.js";

const [recorded] = JSON.parse(document.getElementById("runs").textContent);
const player = mount(document.getElementById("run"), recorded, { label: "replicated register · bug on", autoplay: false });
const report = document.getElementById("debug-report");
const replay = document.getElementById("debug-replay");
const back = document.getElementById("debug-back");
const reset = document.getElementById("debug-reset");
const walk = document.getElementById("debug-walk");
let taken = 0;

function drawReport(run) {
  const v = run.report.first_violation;
  const line = (text, cls) => Object.assign(document.createElement("div"), { textContent: text, className: cls || "" });
  report.replaceChildren(
    line("seed " + run.config.seed + "  ✗ failed", "bad"),
    line("first violation at " + (v ? v.time_ms : "?") + " ms, event " + (v ? model.violationAt(run) : "?") + ": " + (v ? v.message : "")),
    ...run.report.violations.map((t) => line(t))
  );
}

function drawWalk() {
  const steps = model.walkBack(player.run);
  walk.replaceChildren(
    ...steps.slice(0, taken).map((s, i) => {
      const li = document.createElement("li");
      li.textContent = s.text + " (event " + s.index + ")";
      if (i === taken - 1) li.className = "current";
      return li;
    })
  );
  back.disabled = taken >= steps.length;
  back.textContent = taken === 0 ? "Start the walk back" : taken >= steps.length ? "The walk is done" : "Step back";
}

back.addEventListener("click", () => {
  const steps = model.walkBack(player.run);
  if (taken >= steps.length) return;
  taken += 1;
  player.pause();
  player.seek(steps[taken - 1].index);
  drawWalk();
});
reset.addEventListener("click", () => {
  taken = 0;
  player.seek(player.run.events.length - 1);
  drawWalk();
});
replay.addEventListener("click", () => player.runLive(recorded.config));
document.getElementById("run").addEventListener("run", (e) => {
  drawReport(e.detail);
  const steps = model.walkBack(e.detail);
  if (taken > 0 && steps[taken - 1]) player.seek(steps[taken - 1].index);
  drawWalk();
});
drawReport(recorded);
drawWalk();
