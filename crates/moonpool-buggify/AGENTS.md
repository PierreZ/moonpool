# moonpool-buggify

The standalone fault-injection macros and their state, so a library can carry
BUGGIFY sites, hints and shard declarations without depending on the
simulation runtime.

## Surface

| Item | Role |
|---|---|
| `buggify!()`, `buggify_with_prob!(p)` | fire at 25% or at the caller's rate while the site is active |
| `buggify_fault_with_prob!(p)` | a site that makes an operation fail; silent after `buggify_enter_recovery` |
| `buggify_named!("label", p)` | a site a harness switches per seed with `set_activation` |
| `buggify_pick!(p, n)`, `buggify_range!(p, range)` | `Option<usize>` in `0..n`, `Option<u64>` in `range` |
| `hint!("moment"[, p])` (`hint` module) | the code names a moment; the sim's `HintSink` decides (a reboot under the seed's attrition regime) |
| `shard!(name, current = r1[, next = r2])` (`shard` module) | the code declares its replication; the sim's shard book and gate keep faults survivable |
| `cpu_charge(duration)` (`cpu` module) | shipped code reports work it did; a simulator with a CPU model makes it take time |
| `set_random_source`, `clear_random_source`, `buggify_init`, `buggify_reset`, `is_simulated` | the state that moonpool-sim installs per run |

## Rules

- **Pure std, zero dependencies, wasm-able.** Keep `[dependencies]` empty.
- **Inert by default.** With no source installed the macros do nothing: the
  buggify macros return `false` or `None`, `hint!` and `shard!` are no-ops.
  So production builds carry the sites at zero behavioral cost, and
  removing every site leaves the program unchanged. moonpool-sim
  installs its seeded source per run (`buggify_init`) and uninstalls it
  after (`buggify_reset`).
- **The model is two-phase and per location**: a location (`file:line`) is
  activated once per run with the installed activation probability (0.5 in
  the runner), then fires per call at 25% (`buggify!`) or the caller's rate.
  Do not add a global "always fire" switch; per-seed activation is what lets
  the swarm isolate one site's effect.
- **Draw counts are a contract.** A site that fires makes exactly one draw
  more for `buggify_pick!` and `buggify_range!`; a change to the number of
  draws or to the value of a draw moves every seed.
- `buggify_knob!` (value perturbation within a range) deliberately stays in
  moonpool-sim because it needs the ranged draw; do not move it here.
- Book: `book/src/part3-building/08-buggify.md`.
