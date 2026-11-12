// The engine of the site in a Web Worker (#416 (engine and player)): the
// moonpool simulation compiled to wasm (crates/moonpool-wasm-demo, staged by
// scripts/build-wasm-demo.sh). A run blocks this thread, not the page.
//
// In: { id, config, op } (op "run", the default, or "coverage").
// Out: { id, json, ms } or { id, error }.
import init, { coverage, run } from "./wasm-demo/pkg/moonpool_wasm_demo.js";

const ready = init().then(
  () => null,
  (e) => "the wasm engine did not load: " + e
);

self.onmessage = async (msg) => {
  const { id, config, op } = msg.data;
  const failed = await ready;
  if (failed) {
    self.postMessage({ id, error: failed });
    return;
  }
  const t0 = performance.now();
  const json = (op === "coverage" ? coverage : run)(JSON.stringify(config));
  self.postMessage({ id, json, ms: performance.now() - t0 });
};
