// The page side of the engine (#416 (engine and player)): one Web Worker per
// page, made on the first need, so the wasm loads only when a run is near the
// screen or at the first click.
let worker = null;
let next = 0;
const waiting = new Map();

function fail(error) {
  for (const w of waiting.values()) w.reject(new Error(error));
  waiting.clear();
}

function start() {
  if (worker) return worker;
  worker = new Worker(new URL("./engine-worker.js", import.meta.url), { type: "module" });
  worker.onmessage = (msg) => {
    const { id, json, ms, error } = msg.data;
    const w = waiting.get(id);
    if (!w) return;
    waiting.delete(id);
    if (error) return w.reject(new Error(error));
    const run = JSON.parse(json);
    if (run.error) return w.reject(new Error(run.error));
    w.resolve({ run, ms });
  };
  worker.onerror = (e) => {
    e.preventDefault();
    fail("the engine worker stopped: " + (e.message || "no message"));
  };
  return worker;
}

/** Load the worker and the wasm now, before the first run. */
export function warm() {
  start();
}

/** Run one config in the worker: resolves to `{ run, ms }`. */
export function runConfig(config) {
  return new Promise((resolve, reject) => {
    const id = ++next;
    waiting.set(id, { resolve, reject });
    start().postMessage({ id, config });
  });
}
