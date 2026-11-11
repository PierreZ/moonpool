#!/usr/bin/env bash
# Build the wasm ping-pong demo (crates/moonpool-wasm-demo) and stage it in
# the site: public/wasm-demo/ holds the demo page (the iframe of the chapter
# "Simulation in the Browser") and its bundle (the live run of the home page).
# Run it before `npm run build` (locally and in CI). public/wasm-demo/ is
# generated and ignored by git.
set -euo pipefail
cd "$(dirname "$0")/../../.."

echo "building moonpool-wasm-demo for wasm32…"
cargo build --release --target wasm32-unknown-unknown -p moonpool-wasm-demo --lib
wasm-bindgen --target web --out-dir crates/moonpool-wasm-demo/web/pkg \
  target/wasm32-unknown-unknown/release/moonpool_wasm_demo.wasm

out=website/site/public/wasm-demo
echo "staging the demo in $out/…"
rm -rf "$out"
mkdir -p "$out"
cp crates/moonpool-wasm-demo/web/index.html "$out/index.html"
cp -r crates/moonpool-wasm-demo/web/pkg "$out/pkg"

echo "done. $out/ is ready for npm run build."
