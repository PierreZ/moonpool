// The live run of the Assertions page (#418 (technique pages, part 2)): the
// checks of the register turn green at their first hit, seed after seed,
// until the coverage is stable or the cap. The engine decides when to stop
// (crates/moonpool-wasm-demo/src/coverage.rs); this page plays its result.
import * as engine from "./engine.js";
import { reduceMotion } from "./player.js";

const recorded = JSON.parse(document.getElementById("coverage").textContent);
const table = document.getElementById("check-table");
const bar = document.getElementById("check-bar");
const barText = document.getElementById("check-bar-text");
const out = document.getElementById("check-out");
const form = document.getElementById("check-form");
const flip = document.getElementById("check-flip");
let timer = null;

function el(tag, text, cls) {
  const e = document.createElement(tag);
  if (text !== undefined) e.textContent = text;
  if (cls) e.className = cls;
  return e;
}

/** Draw the result after its first `n` seeds. */
function draw(cov, n) {
  const seeds = cov.seeds.slice(0, n);
  const first = new Map();
  for (const s of seeds) for (const c of s.new) first.set(c, s.seed);
  const head = el("tr");
  head.append(el("th", "check"), el("th", "first hit"));
  const rows = cov.checks.map((c) => {
    const tr = el("tr");
    const hit = first.has(c);
    const done = n >= cov.seeds.length;
    tr.append(
      el("td", (hit ? "✓ " : done ? "✗ " : "· ") + c, hit ? "on" : done ? "bad" : "later"),
      el("td", hit ? "seed " + first.get(c) : done ? "no seed" : "not yet", hit ? "" : done ? "bad" : "later")
    );
    return tr;
  });
  table.replaceChildren(head, ...rows);
  const pct = Math.round((100 * first.size) / cov.checks.length);
  bar.style.width = pct + "%";
  bar.className = "bar-fill" + (n >= cov.seeds.length && cov.missing.length ? " bad" : "");
  barText.textContent = first.size + " of " + cov.checks.length + " checks · " + seeds.length + " seeds";
  if (n < cov.seeds.length) {
    out.className = "run-out";
    out.textContent = "Seed " + (seeds.length ? seeds.at(-1).seed : cov.config.run.seed) + "…";
    return;
  }
  if (cov.stable) {
    out.className = "run-out same";
    out.textContent = "✓ Stable after " + cov.seeds.length + " seeds: each check fired, and the last " + cov.config.plateau + " seeds added nothing. The run passes.";
  } else {
    out.className = "run-out bad";
    out.textContent = cov.missing.length
      ? "✗ Stopped at the cap of " + cov.config.max_seeds + " seeds. No seed reached “" + cov.missing.join("”, “") + "”. The run fails: a check that never fires tested nothing."
      : "✗ Stopped at the cap of " + cov.config.max_seeds + " seeds: each check fired, but the coverage was not stable yet. The run fails; a larger cap would let it settle.";
  }
}

function play(cov) {
  clearInterval(timer);
  if (reduceMotion.matches) return draw(cov, cov.seeds.length);
  let n = 0;
  timer = setInterval(() => {
    n++;
    draw(cov, n);
    if (n >= cov.seeds.length) clearInterval(timer);
  }, 40);
}

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  clearInterval(timer);
  const run = { ...recorded.config.run };
  if (flip.checked) run.network_faults = null;
  out.className = "run-out";
  out.textContent = "Running up to " + recorded.config.max_seeds + " seeds in your browser…";
  try {
    const r = await engine.runCoverage({ ...recorded.config, run });
    play(r.run);
  } catch (err) {
    out.className = "run-out bad";
    out.textContent = "The engine did not run: " + err.message + ".";
  }
});

draw(recorded, recorded.seeds.length);
