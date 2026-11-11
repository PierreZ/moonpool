#!/usr/bin/env bash
# Build the run engine of the site (crates/moonpool-wasm-demo) for wasm32 and
# stage its bundle in public/wasm-demo/pkg/. The engine worker
# (public/engine-worker.js) loads it. Run it before `npm run build` (locally
# and in CI). public/wasm-demo/ is generated and ignored by git.
set -euo pipefail
cd "$(dirname "$0")/../../.."

out=website/site/public/wasm-demo/pkg
echo "building moonpool-wasm-demo for wasm32…"
cargo build --release --target wasm32-unknown-unknown -p moonpool-wasm-demo --lib
rm -rf "$out"
mkdir -p "$out"
wasm-bindgen --target web --out-dir "$out" \
  target/wasm32-unknown-unknown/release/moonpool_wasm_demo.wasm
echo "done. $out/ is ready for npm run build."
