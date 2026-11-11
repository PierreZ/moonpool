// The live run of the home page (#415 (new site with the book)).
//
// It draws a run of the ping-pong demo (crates/moonpool-wasm-demo): a lane per
// node, the time axis in simulated milliseconds, each message as a line from
// its departure to its arrival. Every fact comes from the run: the page first
// draws the runs recorded by `cargo test -p moonpool-wasm-demo --test
// site_runs`, then loads the demo compiled to wasm and runs the seeds in the
// browser. No simulation logic is in this file.
//
// The address holds the seed (`?seed=256`), so a shared link shows the same run.
// With "reduce motion" on, nothing plays alone: the visitor steps.
(function () {
  var $ = function (id) { return document.getElementById(id); };
  var script = document.currentScript;
  var NS = "http://www.w3.org/2000/svg";
  var REAL_MS = 7000;
  var reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;

  var recorded = {};
  JSON.parse($("runs").textContent).forEach(function (r) { recorded[r.seed] = r; });
  var buttons = Array.prototype.slice.call(document.querySelectorAll(".run-ctl [data-seed]"));
  var SEEDS = buttons.map(function (b) { return Number(b.dataset.seed); });
  var runSeed = null; // the wasm entry point, once loaded

  var asked = Number(new URLSearchParams(location.search).get("seed"));
  var seed = Number.isSafeInteger(asked) && asked >= 0 && new URLSearchParams(location.search).has("seed") ? asked : 256;
  var run = recorded[seed] || recorded[256];
  var runNo = 1, T = 0, playing = false, last = 0, raf = 0;

  // FNV-1a over the run's JSON: the same seed gives the same bytes, so the same print.
  function fingerprint(r) {
    var s = JSON.stringify(r), h = 0x811c9dc5;
    for (var i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 0x01000193); }
    return (h >>> 0).toString(16).padStart(8, "0");
  }
  function el(name, attrs, text) {
    var e = document.createElementNS(NS, name);
    for (var k in attrs) e.setAttribute(k, attrs[k]);
    if (text != null) e.textContent = text;
    return e;
  }
  function css(v) { return "var(--" + v + ")"; }

  function draw() {
    var svg = $("stage");
    var W = svg.clientWidth || 700, H = 210;
    var narrow = W < 480;
    var left = narrow ? 34 : 96, right = W - 14, yA = 52, yB = 148;
    var total = Math.max(1, run.sim_duration_ms);
    var X = function (ms) { return left + (right - left) * (ms / total); };
    svg.setAttribute("viewBox", "0 0 " + W + " " + H);
    svg.replaceChildren();
    [[yA, "A", "client · Workload"], [yB, "B", "server · Process"]].forEach(function (lane) {
      svg.append(el("line", { x1: left, x2: right, y1: lane[0], y2: lane[0], stroke: css("rule-strong"), "stroke-width": 1 }));
      svg.append(el("text", { x: narrow ? 6 : 10, y: lane[0] + 5, fill: css("fg"), "font-size": 14, "font-weight": 600, "font-family": "var(--font-mono)" }, lane[1]));
      if (!narrow) svg.append(el("text", { x: 26, y: lane[0] + (lane[0] === yA ? -12 : 22), fill: css("muted"), "font-size": 11, "font-family": "var(--font-mono)" }, lane[2]));
    });
    svg.append(el("text", { x: left, y: H - 8, fill: css("info"), "font-size": 11, "font-family": "var(--font-mono)" }, "0 ms"));
    svg.append(el("text", { x: right, y: H - 8, fill: css("info"), "font-size": 11, "text-anchor": "end", "font-family": "var(--font-mono)" }, run.sim_duration_ms + " ms simulated"));
    run.shots.forEach(function (s) {
      var dep = s.depart_ms, arr = s.arrive_ms;
      if (dep > T) return;
      var y1 = s.from === 0 ? yA : yB, y2 = s.from === 0 ? yB : yA;
      if (s.outcome === "delivered") {
        var p = Math.min(1, (T - dep) / Math.max(1, arr - dep));
        var x1 = X(dep), x2 = x1 + (X(arr) - x1) * p, yy = y1 + (y2 - y1) * p;
        svg.append(el("line", { x1: x1, y1: y1, x2: x2, y2: yy, stroke: css("accent"), "stroke-width": 1.5, opacity: p < 1 ? 1 : 0.75 }));
        if (p < 1) svg.append(el("circle", { cx: x2, cy: yy, r: 4, fill: css("accent") }));
      } else {
        // The request got no answer: the client waits until its timeout.
        var xs = X(dep), xe = X(Math.min(arr, T));
        var lostAt = Math.min(arr, dep + Math.max(18, (arr - dep) * 0.12));
        var q = Math.min(1, (T - dep) / Math.max(1, lostAt - dep));
        var xl = xs + (X(lostAt) - xs) * q, yl = y1 + (y2 - y1) * 0.5 * q;
        svg.append(el("line", { x1: xs, y1: y1, x2: xl, y2: yl, stroke: css("fault"), "stroke-width": 1.5, "stroke-dasharray": "3 3" }));
        if (q >= 1) svg.append(el("text", { x: xl, y: yl + 5, fill: css("fault"), "font-size": 14, "font-weight": 700, "text-anchor": "middle", "font-family": "var(--font-mono)" }, "✗"));
        svg.append(el("line", { x1: xs, x2: xe, y1: y1 - 7, y2: y1 - 7, stroke: css("fault"), "stroke-width": 3, opacity: 0.55 }));
        if (T >= arr && X(arr) - xs > 54) svg.append(el("text", { x: (xs + X(arr)) / 2, y: y1 - 13, fill: css("fault"), "font-size": 10.5, "text-anchor": "middle", "font-family": "var(--font-mono)" }, "no answer"));
      }
    });
    if (T < total) svg.append(el("line", { x1: X(T), x2: X(T), y1: 22, y2: H - 26, stroke: css("info"), "stroke-width": 1, "stroke-dasharray": "2 3" }));
  }

  function meta() {
    $("r-seed").textContent = seed;
    $("r-n").textContent = runNo;
    $("r-src").textContent = runSeed ? "in your browser" : "recorded";
    var cell = function (k, v, c) { return '<div><span class="k">' + k + '</span><span class="v ' + (c || "") + '">' + v + "</span></div>"; };
    $("stats").innerHTML =
      cell("Requests", run.requests) +
      cell("Delivered", run.delivered, "ok") +
      cell("No answer", run.dropped, run.dropped ? "fault" : "") +
      cell("Network faults", run.faults, run.faults ? "fault" : "") +
      cell("Slowest round trip", run.longest_rtt_ms + " ms") +
      cell("Simulated time", run.sim_duration_ms + " ms", "time");
    var rec = recorded[seed];
    var fp = fingerprint(run);
    var foot = "<span>fingerprint <b>" + fp + "</b></span>";
    if (runNo > 1) foot += '<span class="same">run ' + runNo + ": the same seed gave the same run.</span>";
    else if (runSeed && rec) foot += '<span class="same">' + (fingerprint(rec) === fp ? "the same as the recorded run." : "not the recorded run.") + "</span>";
    $("foot").innerHTML = foot;
    $("log").textContent = run.shots.map(function (s) {
      return "t=" + String(s.depart_ms).padStart(5) + " ms  req " + String(s.seq).padStart(2) + "  " +
        (s.from === 0 ? "A → B" : "B → A") + "  " +
        (s.outcome === "delivered" ? "delivered after " + s.latency_ms + " ms" : "no answer, the client gave up after " + s.latency_ms + " ms");
    }).join("\n");
    buttons.forEach(function (b) { b.setAttribute("aria-pressed", String(Number(b.dataset.seed) === seed)); });
  }

  function tick(now) {
    if (!playing) return;
    T = Math.min(run.sim_duration_ms, T + (now - last) * run.sim_duration_ms / REAL_MS);
    last = now;
    draw();
    if (T >= run.sim_duration_ms) { pause(); return; }
    raf = requestAnimationFrame(tick);
  }
  function play() {
    if (T >= run.sim_duration_ms) T = 0;
    playing = true; last = performance.now(); $("play").textContent = "Pause";
    cancelAnimationFrame(raf); raf = requestAnimationFrame(tick);
  }
  function pause() { playing = false; $("play").textContent = "Play"; cancelAnimationFrame(raf); }

  function load(s) {
    if (runSeed) return JSON.parse(runSeed(BigInt(s)));
    return recorded[s];
  }
  function start(s, again) {
    var next = load(s);
    if (!next) return;
    pause();
    runNo = again && s === seed ? runNo + 1 : 1;
    seed = s; run = next;
    var u = new URL(location.href);
    u.searchParams.set("seed", String(s));
    history.replaceState(null, "", u);
    meta();
    if (reduce) { T = run.sim_duration_ms; draw(); } else { T = 0; play(); }
  }

  buttons.forEach(function (b) { b.addEventListener("click", function () { start(Number(b.dataset.seed), false); }); });
  $("play").addEventListener("click", function () { playing ? pause() : play(); });
  $("step").addEventListener("click", function () {
    pause();
    var next = run.shots.map(function (s) { return s.arrive_ms; }).filter(function (a) { return a > T + 0.5; }).sort(function (a, b) { return a - b; })[0];
    T = next == null ? 0 : next;
    draw();
  });
  $("again").addEventListener("click", function () { start(seed, true); });
  $("hero-run").addEventListener("click", function () {
    var i = SEEDS.indexOf(seed);
    start(SEEDS[(i + 1) % SEEDS.length], false);
    $("run").scrollIntoView({ block: "center", behavior: reduce ? "auto" : "smooth" });
  });
  $("seed-form").addEventListener("submit", function (e) {
    e.preventDefault();
    var v = $("seed-input").value.trim();
    // A seed is a u64; the page takes the ones that a JavaScript number holds exactly.
    if (/^\d{1,16}$/.test(v) && Number.isSafeInteger(Number(v))) start(Number(v), false);
  });
  addEventListener("resize", draw);
  addEventListener("paros-theme", draw);

  // At rest the page shows the whole run. Then it plays once, unless motion is reduced.
  meta(); T = run.sim_duration_ms; draw();
  if (!reduce) setTimeout(function () { if (!playing && runNo === 1) { T = 0; play(); } }, 900);

  // Then the simulator itself: the demo compiled to wasm runs any seed here.
  import(script.dataset.wasm).then(function (m) {
    return m.default().then(function () {
      runSeed = m.runSeed;
      $("seed-form").hidden = false;
      if (!recorded[seed]) start(seed, false); else meta();
    });
  }).catch(function () { /* the recorded runs stay */ });
})();
