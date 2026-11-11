// The live run of the Determinism page (#416 (engine and player)): one seed
// run two times, side by side. The left player drives the right one, so they
// step together; the first event that differs gets the ≠ mark. The address
// holds the seed (`?seed=256`).
import { Player, reduceMotion, setActive } from "./player.js";
import * as engine from "./engine.js";
import * as model from "./run-model.js";

const [recorded] = JSON.parse(document.getElementById("runs").textContent);
const form = document.getElementById("compare-form");
const inA = document.getElementById("seed-a");
const inB = document.getElementById("seed-b");
const out = document.getElementById("compare-out");
const label = "replicated register";
const left = new Player(document.getElementById("run-a"), { run: recorded, label, seedField: false, address: false, logOpen: true });
const right = new Player(document.getElementById("run-b"), { run: recorded, label, controls: false, address: false, logOpen: true });
left.onCursor((i) => right.seek(i));
right.root.addEventListener("pointerdown", () => setActive(left));

function compare(times) {
  const a = left.run;
  const b = right.run;
  const d = model.firstDifference(a, b);
  left.setDiff(d);
  right.setDiff(d);
  const clock = times ? ` Simulated ${(a.report.sim_time_ms / 1000).toFixed(2)} s in ${Math.round(times[0])} ms and ${Math.round(times[1])} ms of real time.` : "";
  if (d < 0) {
    out.className = "compare-out same";
    out.textContent = `= All ${a.events.length} events are the same, one by one. Same print ${a.fingerprint.events}/${a.fingerprint.draws}.` + clock;
  } else {
    const e = (a.events[d] || b.events[d]);
    out.className = "compare-out diff";
    const same = d === 1 ? "the first event" : `the first ${d} events`;
    out.textContent = (d === 0 ? "≠ The runs differ from the first event" : `≠ The runs are the same for ${same}`) + `. Event ${d + 1} (at ${e.time_ms} ms) is the first that differs.` + clock;
  }
}

async function runBoth(sa, sb) {
  out.className = "compare-out";
  out.textContent = `Running seed ${sa} and seed ${sb} in your browser…`;
  try {
    const a = await engine.runConfig({ ...recorded.config, seed: sa });
    const b = await engine.runConfig({ ...recorded.config, seed: sb });
    left.show(a.run, { source: "live", ms: a.ms });
    right.show(b.run, { source: "live", ms: b.ms });
    compare([a.ms, b.ms]);
    const u = new URL(location.href);
    u.searchParams.set("seed", String(sa));
    if (sb !== sa) u.searchParams.set("right", String(sb));
    else u.searchParams.delete("right");
    history.replaceState(null, "", u);
    return true;
  } catch (e) {
    out.className = "compare-out diff";
    out.textContent = "The engine did not run: " + e.message + ". The recorded runs stay.";
    return false;
  }
}

function seeds() {
  const a = model.seedFrom("?seed=" + inA.value);
  const b = model.seedFrom("?seed=" + inB.value);
  if (a === null || b === null) {
    out.className = "compare-out diff";
    out.textContent = "A seed is a whole number, for example 256.";
    return null;
  }
  return [a, b];
}

form.addEventListener("submit", (e) => {
  e.preventDefault();
  const s = seeds();
  if (s) runBoth(...s);
});
document.getElementById("seed-next").addEventListener("click", () => {
  const s = seeds();
  if (!s) return;
  inB.value = String(s[1] === s[0] ? s[0] + 1 : s[1] + 1);
  runBoth(s[0], Number(inB.value));
});

compare(null);
const askedA = model.seedFrom(location.search);
const askedB = model.seedFrom("?seed=" + (new URLSearchParams(location.search).get("right") || ""));
if (askedA !== null) inA.value = String(askedA);
inB.value = String(askedB !== null ? askedB : Number(inA.value));

let started = false;
const root = document.getElementById("compare");
root.addEventListener("pointerdown", () => engine.warm(), { once: true });
new IntersectionObserver((entries, obs) => {
  if (started || !entries.some((e) => e.isIntersecting)) return;
  started = true;
  obs.disconnect();
  const s = seeds();
  if (!s) return;
  runBoth(...s).then((ok) => {
    if (ok && !reduceMotion.matches) left.play();
  });
}, { rootMargin: "200px" }).observe(root);
