# moonpool-buggify

The standalone `buggify!` / `buggify_with_prob!` macros and their state, so a
library can carry BUGGIFY sites without depending on the simulation runtime.

- **Pure std, zero dependencies, wasm-able.** Keep `[dependencies]` empty.
- **Inert by default.** With no source installed both macros return `false`,
  so production builds carry the sites at zero behavioral cost and removing
  every site leaves the program unchanged. moonpool-sim installs its seeded
  source per run (`buggify_init`) and uninstalls it after (`buggify_reset`).
- **The model is two-phase and per location**: a location (`file:line`) is
  activated once per run with the installed activation probability (0.5 in
  the runner), then fires per call at 25% (`buggify!`) or the caller's rate.
  Do not add a global "always fire" switch; per-seed activation is what lets
  the swarm isolate one site's effect.
- `buggify_knob!` (value perturbation within a range) deliberately stays in
  moonpool-sim because it needs the ranged draw; do not move it here.
- Book: `book/src/part3-building/08-buggify.md`.
