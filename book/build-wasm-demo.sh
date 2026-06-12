#!/usr/bin/env bash
# Build the wasm ping-pong demo and stage it as a book asset so the wasm chapter
# can embed it live. Run before `mdbook build` (locally and in CI). The staged
# directory book/src/wasm-demo/ is generated and gitignored.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "building moonpool-wasm-demo for wasm32…"
cargo build --release --target wasm32-unknown-unknown -p moonpool-wasm-demo --lib
wasm-bindgen --target web --out-dir moonpool-wasm-demo/web/pkg \
  target/wasm32-unknown-unknown/release/moonpool_wasm_demo.wasm

echo "staging assets in book/src/wasm-demo/…"
rm -rf book/src/wasm-demo
mkdir -p book/src/wasm-demo
cp moonpool-wasm-demo/web/index.html book/src/wasm-demo/index.html
cp -r moonpool-wasm-demo/web/pkg book/src/wasm-demo/pkg

echo "done. book/src/wasm-demo/ ready for mdbook build."
