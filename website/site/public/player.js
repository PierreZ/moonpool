// The player of the site (#416 (engine and player)): it draws one run of the
// engine as SVG. One lane per process on a time axis in simulated
// milliseconds. A message is a dot that travels from lane to lane. An
// injected fault is an amber mark, a process that is down a red span, a read
// that the invariant checked a green tick, and a broken invariant a red mark
// with its message. The same events are a text list under the picture.
//
// Every fact comes from the run (public/run-model.js reads it). With "reduce
// motion" on, nothing plays alone: the visitor steps.
import * as model from "./run-model.js";
import * as engine from "./engine.js";

const NS = "http://www.w3.org/2000/svg";
const SPEEDS = [0.25, 0.5, 1, 2];
// Simulated milliseconds per real second at speed 1.
const BASE_RATE = 400;
export const reduceMotion = matchMedia("(prefers-reduced-motion: reduce)");

function svg(name, attrs, text) {
  const e = document.createElementNS(NS, name);
  for (const k in attrs) e.setAttribute(k, attrs[k]);
  if (text != null) e.textContent = text;
  return e;
}
function html(name, attrs, text) {
  const e = document.createElement(name);
  for (const k in attrs) e.setAttribute(k, attrs[k]);
  if (text != null) e.textContent = text;
  return e;
}
const v = (name) => "var(--" + name + ")";

// The player that the arrow keys drive: the last one used, else the first.
let active = null;

/** Make the arrow keys drive this player. */
export function setActive(player) {
  active = player;
}

export class Player {
  /**
   * @param {HTMLElement} root an empty `<section class="run">`
   * @param {object} opts
   *   run: the first run (a recorded one)
   *   label: what the run shows, after the seed in the title bar
   *   controls: show the controls (default true)
   *   seedField: show the seed field and "run again" (default true)
   *   address: keep the seed in the address, `?seed=` (default true)
   *   logOpen: open the list of events (default false)
   */
  constructor(root, opts) {
    this.root = root;
    this.opts = { controls: true, seedField: true, address: true, logOpen: false, ...opts };
    this.cursor = -1;
    this.time = 0;
    this.playing = false;
    this.speed = 1;
    this.diffAt = -1;
    this.listeners = [];
    this.prints = new Map(); // config -> the first print seen for it
    this.build();
    this.show(opts.run, { source: "recorded" });
    if (!active) active = this;
    root.addEventListener("pointerdown", () => (active = this));
    root.addEventListener("focusin", () => (active = this));
    new ResizeObserver(() => this.draw()).observe(this.stage);
  }

  /** Call `fn(cursor)` after each change of the cursor. */
  onCursor(fn) {
    this.listeners.push(fn);
  }

  build() {
    const r = this.root;
    r.replaceChildren();
    this.head = html("div", { class: "run-head label" });
    this.title = html("span");
    this.badge = html("span");
    this.head.append(this.title, this.badge);
    const stageBox = html("div", { class: "run-stage" });
    this.stage = svg("svg", {
      role: "img",
      "aria-label": "The lanes of the run. The list of events under it holds the same facts.",
    });
    stageBox.append(this.stage);
    const legend = html("div", { class: "run-legend" });
    legend.innerHTML =
      '<span><i class="m-msg">●</i> message</span><span><i class="m-lost">✕</i> lost</span>' +
      '<span><i class="m-fault">⌁▼▣</i> fault</span><span><i class="m-down">▬</i> down</span>' +
      '<span><i class="m-ok">✓</i> read checked</span><span><i class="m-fail">✗</i> invariant broken</span>';
    r.append(this.head, stageBox, legend);

    if (this.opts.controls) {
      const ctl = html("div", { class: "run-ctl" });
      const btn = (label, aria, fn) => {
        const b = html("button", { class: "btn small", type: "button", "aria-label": aria }, label);
        b.addEventListener("click", fn);
        ctl.append(b);
        return b;
      };
      btn("⏮", "Restart", () => this.restart());
      btn("◀", "Back one event", () => this.step(-1));
      this.playBtn = btn("▶ Play", "Play", () => (this.playing ? this.pause() : this.play()));
      btn("▶|", "Step one event", () => this.step(1));
      const speed = html("select", { "aria-label": "Speed" });
      SPEEDS.forEach((s) => speed.append(html("option", { value: s }, "×" + s)));
      speed.value = "1";
      speed.addEventListener("change", () => (this.speed = Number(speed.value)));
      ctl.append(speed);
      if (this.opts.seedField) {
        ctl.append(html("span", { class: "sep", "aria-hidden": "true" }));
        const form = html("form", { class: "seed-form" });
        const id = "seed-" + Math.random().toString(36).slice(2, 8);
        form.append(html("label", { class: "cap label", for: id }, "seed"));
        this.seedInput = html("input", { id, inputmode: "numeric", pattern: "[0-9]*", maxlength: "15", autocomplete: "off" });
        form.append(this.seedInput, html("button", { class: "btn small", type: "submit" }, "Run"));
        form.addEventListener("submit", (e) => {
          e.preventDefault();
          const s = model.seedFrom("?seed=" + this.seedInput.value);
          if (s === null) {
            this.setNote("A seed is a whole number, for example 256.", "bad");
            return;
          }
          this.runLive({ ...this.run.config, seed: s });
        });
        ctl.append(form);
        btn("Run again", "Run this seed again", () => this.runLive(this.run.config));
      }
      r.append(ctl);
    }

    this.stats = html("div", { class: "run-stats" });
    this.foot = html("div", { class: "run-foot", "aria-live": "polite" });
    this.note = html("span");
    this.verdict = html("span");
    this.foot.append(this.verdict, this.note);
    const log = html("details", { class: "log" });
    if (this.opts.logOpen) log.open = true;
    this.logSummary = html("summary", { class: "label" });
    this.list = html("ol", { class: "events" });
    log.append(this.logSummary, this.list);
    log.addEventListener("toggle", () => this.scrollList());
    this.log = log;
    r.append(this.stats, this.foot, log);
  }

  /** Show a run. `info.source` is "recorded" or "live"; `info.ms` the real time. */
  show(run, info) {
    // A run shown now wins over a live run still on its way (runLive).
    this.ticket = (this.ticket || 0) + 1;
    this.pause();
    this.run = run;
    this.lanes = model.lanes(run);
    this.msgs = model.messages(run);
    this.down = model.downSpans(run);
    this.broken = model.violationAt(run);
    this.diffAt = -1;
    const key = JSON.stringify(run.config);
    const print = run.fingerprint.events + "/" + run.fingerprint.draws;
    const first = this.prints.get(key);
    if (!first) this.prints.set(key, print);

    this.title.innerHTML = "";
    this.title.append("seed ", html("b", {}, String(run.config.seed)), " · " + (this.opts.label || run.config.scenario));
    this.badge.textContent = info.source === "live" ? "live, in your browser" : "recorded run";
    if (this.seedInput) this.seedInput.value = String(run.config.seed);

    const f = model.figures(run);
    this.stats.replaceChildren(
      ...[
        ["simulated", (f.simMs / 1000).toFixed(2) + " s"],
        ["events", f.events],
        ["messages", f.messages],
        ["faults", f.faults],
        ["writes committed", f.committed],
        ["print", f.print.split("/")[0]],
      ].map(([k, val]) => {
        const d = html("div");
        d.append(html("span", { class: "k" }, k), html("span", { class: "v" }, String(val)));
        return d;
      })
    );
    this.verdict.className = "chip-status " + (run.report.passed ? "done" : "gap");
    this.verdict.textContent = run.report.passed
      ? "✓ passed"
      : "✗ " + (run.report.first_violation ? run.report.first_violation.message : run.report.violations[0] || "failed");
    let note = "";
    if (info.source === "live") {
      note = (f.simMs / 1000).toFixed(2) + " s of simulated time in " + Math.max(1, Math.round(info.ms)) + " ms of real time.";
      if (first) note += first === print ? " Same print as the first run of this seed ✓" : " A different print: report it, this is a bug.";
    } else {
      note = "Recorded by a Rust test. The engine runs it again in your browser.";
    }
    this.setNote(note);

    this.logSummary.textContent = "The " + run.events.length + " events of this run";
    this.list.replaceChildren(
      ...run.events.map((e, i) => {
        const li = html("li", {}, model.eventText(e));
        if (e.name === "sim_fault") li.className = "ev-fault";
        if (i === this.broken) li.className = "ev-fail";
        return li;
      })
    );
    if (this.opts.address && info.source === "live") this.writeAddress();
    this.seek(run.events.length - 1);
  }

  setNote(text, kind) {
    this.note.textContent = text;
    this.note.className = kind === "bad" ? "bad" : "";
  }

  writeAddress() {
    const u = new URL(location.href);
    u.searchParams.set("seed", String(this.run.config.seed));
    history.replaceState(null, "", u);
  }

  /**
   * Run a config in the engine and show it. Resolves to the run, or null.
   * The last call wins: a run that comes back after a later runLive or
   * show is dropped, so a slow first run never hides the run the reader
   * asked for.
   */
  async runLive(config) {
    const ticket = (this.ticket = (this.ticket || 0) + 1);
    this.setNote("Running seed " + config.seed + " in your browser…");
    try {
      const { run, ms } = await engine.runConfig(config);
      if (ticket !== this.ticket) return null;
      this.show(run, { source: "live", ms });
      this.dispatchRun(run);
      return run;
    } catch (e) {
      if (ticket !== this.ticket) return null;
      this.setNote("The engine did not run: " + e.message + ". The recorded run stays.", "bad");
      return null;
    }
  }

  dispatchRun(run) {
    this.root.dispatchEvent(new CustomEvent("run", { detail: run }));
  }

  /** Go to an event index (-1 is before the first event). */
  seek(i) {
    const n = this.run.events.length;
    this.cursor = Math.max(-1, Math.min(n - 1, i));
    this.time = this.cursor >= 0 ? this.run.events[this.cursor].time_ms : 0;
    this.changed();
  }

  step(d) {
    this.pause();
    this.seek(this.cursor + d);
  }

  restart() {
    this.pause();
    this.seek(-1);
  }

  /** Mark the first event that differs from another run. */
  setDiff(i) {
    this.diffAt = i;
    [...this.list.children].forEach((li, j) => li.classList.toggle("ev-diff", j === i));
    this.draw();
  }

  play() {
    const evs = this.run.events;
    if (this.cursor >= evs.length - 1) this.seek(-1);
    this.playing = true;
    if (this.playBtn) {
      this.playBtn.textContent = "❚❚ Pause";
      this.playBtn.setAttribute("aria-label", "Pause");
    }
    let last = performance.now();
    const tick = (now) => {
      if (!this.playing) return;
      this.time += ((now - last) / 1000) * BASE_RATE * this.speed;
      last = now;
      while (this.cursor + 1 < evs.length && evs[this.cursor + 1].time_ms <= this.time) this.cursor++;
      if (this.cursor >= evs.length - 1) {
        this.pause();
        this.seek(evs.length - 1);
        return;
      }
      this.changed();
      this.raf = requestAnimationFrame(tick);
    };
    this.raf = requestAnimationFrame(tick);
  }

  pause() {
    this.playing = false;
    cancelAnimationFrame(this.raf);
    if (this.playBtn) {
      this.playBtn.textContent = "▶ Play";
      this.playBtn.setAttribute("aria-label", "Play");
    }
  }

  changed() {
    this.draw();
    const items = this.list.children;
    for (let i = 0; i < items.length; i++) {
      items[i].classList.toggle("later", i > this.cursor);
      if (i === this.cursor) items[i].setAttribute("aria-current", "step");
      else items[i].removeAttribute("aria-current");
    }
    this.scrollList();
    this.listeners.forEach((fn) => fn(this.cursor));
  }

  scrollList() {
    if (!this.log.open) return;
    const li = this.list.children[Math.max(0, this.cursor)];
    if (li) this.list.scrollTop = li.offsetTop - this.list.offsetTop - this.list.clientHeight / 2;
  }

  draw() {
    const run = this.run;
    const W = this.stage.clientWidth || 640;
    const laneGap = 46;
    const top = 38;
    const H = top + laneGap * (this.lanes.length - 1) + 40;
    const left = 10;
    const right = W - 10;
    const T = Math.max(1, run.report.sim_time_ms);
    const X = (ms) => left + ((right - left) * Math.min(ms, T)) / T;
    const Y = new Map(this.lanes.map((l, i) => [l.ip, top + i * laneGap]));
    const shown = (i) => i >= 0 && i <= this.cursor;
    const g = this.stage;
    g.setAttribute("viewBox", "0 0 " + W + " " + H);
    g.style.height = H + "px";
    g.replaceChildren();
    const font = { "font-family": "var(--font-mono)", "font-size": 11 };

    // Lanes, with their name above the line.
    this.lanes.forEach((l) => {
      const y = Y.get(l.ip);
      g.append(svg("line", { x1: left, x2: right, y1: y, y2: y, stroke: v("rule-strong") }));
    });

    // A process that is down: a red span, from its stop to its next boot.
    this.down.forEach((s) => {
      if (!shown(s.start)) return;
      const y = Y.get(s.ip);
      if (y == null) return;
      const end = s.end >= 0 && shown(s.end) ? s.endMs : this.time;
      const x0 = X(s.startMs);
      const x1 = Math.max(x0 + 2, X(end));
      g.append(svg("rect", { x: x0, y: y - 4, width: x1 - x0, height: 8, fill: v("fail-tint"), stroke: v("fail"), "stroke-dasharray": "3 2" }));
      g.append(svg("text", { ...font, x: x0 + 3, y: y + 16, fill: v("fail") }, "▬ down"));
    });

    // Faults: an amber glyph on the lane, or on both ends of a cut link.
    run.events.forEach((e, i) => {
      if (e.name !== "sim_fault" || !shown(i)) return;
      const t = model.faultTarget({ fields: e.fields });
      const x = X(e.time_ms);
      const glyph = model.faultGlyph(e.fields.kind);
      const mark = (y) => {
        const txt = svg("text", { ...font, "font-size": 13, x, y: y + 4, fill: v("fault"), "text-anchor": "middle" }, glyph);
        txt.append(svg("title", {}, e.fields.kind));
        g.append(txt);
      };
      if (t.link) {
        const [a, b] = t.link.map((ip) => Y.get(ip));
        if (a == null || b == null) return;
        g.append(svg("line", { x1: x, x2: x, y1: a, y2: b, stroke: v("fault"), "stroke-dasharray": "2 3" }));
        mark(a);
        mark(b);
      } else if (t.ip && Y.has(t.ip)) {
        const y = Y.get(t.ip);
        if (e.fields.duration_ms) {
          g.append(svg("rect", { x, y: y - 2, width: Math.max(2, X(e.time_ms + e.fields.duration_ms) - x), height: 4, fill: v("fault") }));
        }
        mark(y - 12);
      }
    });

    // Messages: a line from where it left to where it arrived, and a dot in
    // flight at the time of the player.
    this.msgs.forEach((m) => {
      if (!shown(m.sendAt)) return;
      const y0 = Y.get(m.from);
      const y1 = Y.get(m.to);
      if (y0 == null || y1 == null) return;
      const x0 = X(m.sent_ms);
      if (m.received_ms === null) {
        const xm = x0 + 10;
        const ym = (y0 + y1) / 2;
        g.append(svg("line", { x1: x0, y1: y0, x2: xm, y2: ym, stroke: v("muted"), "stroke-dasharray": "2 2" }));
        g.append(svg("text", { ...font, x: xm, y: ym + 4, fill: v("muted"), "text-anchor": "middle" }, "✕"));
        return;
      }
      const x1 = X(m.received_ms);
      const arrived = shown(m.recvAt);
      const f = arrived ? 1 : Math.max(0, Math.min(1, (this.time - m.sent_ms) / Math.max(1, m.received_ms - m.sent_ms)));
      const xe = x0 + (x1 - x0) * f;
      const ye = y0 + (y1 - y0) * f;
      g.append(svg("line", { x1: x0, y1: y0, x2: xe, y2: ye, stroke: v(arrived ? "rule-strong" : "fg"), "stroke-width": arrived ? 1 : 1.5 }));
      if (!arrived) g.append(svg("circle", { cx: xe, cy: ye, r: 3.5, fill: v("fg") }));
    });

    // Reads that the invariant checked: a green tick; the broken one: a red mark.
    run.events.forEach((e, i) => {
      if (!shown(i) || e.name !== "read_returned") return;
      const y = Y.get(e.source);
      const x = X(e.time_ms);
      if (i === this.broken) return;
      g.append(svg("text", { ...font, "font-size": 12, x, y: y + 16, fill: v("accent"), "text-anchor": "middle" }, "✓"));
    });
    if (shown(this.broken)) {
      const e = run.events[this.broken];
      const y = Y.get(e.source);
      const x = X(e.time_ms);
      g.append(svg("line", { x1: x, x2: x, y1: y - 10, y2: H - 22, stroke: v("fail"), "stroke-width": 1.5 }));
      const label = "✗ " + run.report.first_violation.message;
      const anchor = x > W * 0.6 ? "end" : "start";
      g.append(svg("text", { ...font, "font-weight": 600, x: anchor === "end" ? x - 4 : x + 4, y: y + 17, fill: v("fail"), "text-anchor": anchor }, label));
    }

    // The first event that differs from the other run.
    if (this.diffAt >= 0 && shown(this.diffAt)) {
      const e = run.events[this.diffAt];
      const x = X(e.time_ms);
      g.append(svg("line", { x1: x, x2: x, y1: 14, y2: H - 22, stroke: v("fg"), "stroke-dasharray": "4 3" }));
      const anchor = x > W * 0.6 ? "end" : "start";
      g.append(svg("text", { ...font, x: anchor === "end" ? x - 4 : x + 4, y: 11, fill: v("fg"), "text-anchor": anchor }, "≠ first difference"));
    }

    // The names of the lanes, last, on a halo of the panel colour.
    this.lanes.forEach((l) => {
      const label = l.name + " " + l.ip + (l.role === "workload" ? " · workload" : "");
      g.append(svg("text", { ...font, class: "halo", x: left, y: Y.get(l.ip) - 7, fill: v("muted") }, label));
    });

    // The time axis and the play head, in cyan: simulated time.
    const ay = H - 14;
    g.append(svg("line", { x1: left, x2: right, y1: ay - 8, y2: ay - 8, stroke: v("rule") }));
    g.append(svg("text", { ...font, x: left, y: ay + 6, fill: v("muted") }, "0 ms"));
    g.append(svg("text", { ...font, x: right, y: ay + 6, fill: v("muted"), "text-anchor": "end" }, T + " ms"));
    const xh = X(this.time);
    g.append(svg("line", { x1: xh, x2: xh, y1: 14, y2: ay - 8, stroke: v("info") }));
    const hl = "t = " + Math.round(this.time) + " ms";
    const ha = xh > W * 0.75 ? "end" : xh < W * 0.25 ? "start" : "middle";
    if (Math.abs(xh - left) > 40 || this.cursor >= 0) {
      g.append(svg("rect", { x: ha === "end" ? xh - 74 : ha === "start" ? xh : xh - 37, y: ay - 6, width: 74, height: 15, fill: v("panel") }));
      g.append(svg("text", { ...font, x: xh, y: ay + 6, fill: v("info"), "text-anchor": ha }, hl));
    }
  }
}

// The arrow keys step the active player, when no field has the focus.
addEventListener("keydown", (e) => {
  if (!active || e.altKey || e.ctrlKey || e.metaKey) return;
  const tag = (e.target && e.target.tagName) || "";
  if (/INPUT|SELECT|TEXTAREA/.test(tag)) return;
  if (e.key === "ArrowRight") active.step(1);
  else if (e.key === "ArrowLeft") active.step(-1);
  else return;
  e.preventDefault();
});

/**
 * Show a page's recorded run, then the live engine: when the run is near the
 * screen, load the wasm and run the same config again. Without reduce
 * motion, the run plays once when it is first in view.
 */
export function mount(root, run, opts) {
  const player = new Player(root, { run, ...opts });
  const first = player.ticket;
  const asked = opts.address === false ? null : model.seedFrom(location.search);
  let started = false;
  const begin = () => {
    if (started) return;
    started = true;
    // The reader already chose another run: keep it.
    if (player.ticket !== first) return;
    engine.warm();
    const config = asked !== null ? { ...run.config, seed: asked } : run.config;
    player.runLive(config).then((r) => {
      if (r && !reduceMotion.matches && opts.autoplay !== false) player.play();
    });
  };
  root.addEventListener("pointerdown", () => engine.warm(), { once: true });
  new IntersectionObserver((entries, obs) => {
    if (entries.some((e) => e.isIntersecting)) {
      obs.disconnect();
      begin();
    }
  }, { rootMargin: "200px" }).observe(root);
  return player;
}
