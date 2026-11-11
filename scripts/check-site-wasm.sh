#!/usr/bin/env bash
# One config of the site's engine is the same run on wasm32 as natively
# (#416 (engine and player)): the browser shows the run that a test records.
#
#   scripts/check-site-wasm.sh <seed>...
#
# Builds the engine (crates/moonpool-wasm-demo) natively and for wasm32, runs
# each seed through both (the wasm bundle under node), with the planted bug
# off and on, and fails when any line differs. Run it inside `nix develop`
# (wasm-bindgen-cli and node come from the flake).
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <seed>..." >&2
  exit 2
fi

OUT=target/site-wasm-check
rm -rf "$OUT"
mkdir -p "$OUT"

configs=()
for seed in "$@"; do
  configs+=("{\"seed\":$seed}" "{\"seed\":$seed,\"bug\":true}")
done

echo "building the engine natively and for wasm32-unknown-unknown…"
cargo build --release -p moonpool-wasm-demo --bin site-run
cargo build --release --target wasm32-unknown-unknown -p moonpool-wasm-demo --lib
wasm-bindgen --target nodejs --no-typescript --out-dir "$OUT/pkg" \
  target/wasm32-unknown-unknown/release/moonpool_wasm_demo.wasm

echo "running ${#configs[@]} configs natively…"
target/release/site-run "${configs[@]}" >"$OUT/native.txt"
echo "running ${#configs[@]} configs in wasm under node…"
node -e '
  const engine = require(process.argv[1]);
  for (const c of process.argv.slice(2)) process.stdout.write(engine.run(c) + "\n");
' "$PWD/$OUT/pkg/moonpool_wasm_demo.js" "${configs[@]}" >"$OUT/wasm.txt"

if ! cmp -s "$OUT/native.txt" "$OUT/wasm.txt"; then
  diff <(tr ',' '\n' <"$OUT/native.txt") <(tr ',' '\n' <"$OUT/wasm.txt") | head -40
  echo "a config ran differently on wasm32 than natively" >&2
  exit 1
fi
node -e '
  for (const l of require("fs").readFileSync(process.argv[1], "utf8").trim().split("\n")) {
    const r = JSON.parse(l);
    console.log(`seed ${r.config.seed} bug ${r.config.bug}: ${r.events.length} events, print ${r.fingerprint.events}/${r.fingerprint.draws}, passed ${r.report.passed}`);
  }
' "$OUT/native.txt"
echo "every config is the same run on wasm32 as natively"
