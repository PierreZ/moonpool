# moonpool-walk

A seeded random walk over sans-I/O state machines: the judge between a model
checker and a `moonpool-sim` simulation. No executor, no network, no clock.

## Surface

| Item | Role |
|---|---|
| `Model` | what a user writes: `new`, `deliver`, and optionally `actions`/`weight`/`act`, `nodes`/`is_up`/`node_weight`/`crash`/`restart`, `calm`, `check`, `converged`, `finish`, `describe`, `reachable_sites`/`sometimes_sites` |
| `Cx` | what a model reaches while it is called: `rng`, `send`, `record`, `crashed`, `drop_where`, `inflight`, `phase`, `phase_step`, `step`, `seed` |
| `Walk` | the seeded schedule (chaos, quiet, settle) and the campaign: `run`, `report`, `try_run`, `run_each`, `replay`, `check_determinism`, and `finish` for the tail of a scripted `Harness` |
| `Harness` | the step engine, public for scripted schedules; `Walk` drives the same one |
| `Run`, `Logged`, `Entry`, `Stats` | everything one seed did |
| `Report`, `WalkError`, `SeedFailure`, `Site` | the result of a campaign |
| `Rng`, `Mailbox` | splitmix64 and the bounded bag of messages in flight |

## Rules

- **Dependencies: `moonpool-assertions` and `moonpool-buggify` only.** Both
  have zero dependencies. The crate must compile on
  `wasm32-unknown-unknown`. It must never depend on `moonpool-sim`,
  `moonpool-core` or a trace checker: a checker reads `Run::events`, the
  walk does not know it. `moonpool-tla` is a dev-dependency only:
  `tests/paxos.rs` gives a walk's events to TLC (spec in `tests/paxos_tla`).
- **One generator for each run.** The schedule and the model draw from
  `Core::rng`. Buggify draws from its own stream (`seed ^ BUGGIFY_SALT`), so
  a new buggify site does not move the schedule of a seed. The swarm draws
  its mask from another (`seed ^ SWARM_SALT`), but a swarm changes the
  schedule: a family that is off draws nothing.
- **Draw counts are a contract.** A change to the number or the order of the
  draws in `Walk::chaos` and `Walk::tail` moves every seed of every user. Say so in the PR.
- **The model holds the truth about who is up** (`Model::is_up`). The walk
  keeps no list of dead nodes: a node may crash by itself inside `deliver`.
- **The walk owns thread-local state only for the length of a campaign**
  (`Installed`): the assertion table when none was installed, and buggify
  when no simulation was running. It removes both on drop. In a table that
  is not its own it counts, but it declares no site and judges no coverage.
  In a simulation it does not touch buggify, `buggify_enter_recovery`
  included.
- **A read of the assertion table scans all its slots**, so the walk reads
  it once for each seed. When that read finds a violation, `one_seed` runs
  the seed again with a read after every step, to report the right step.
- **A failure is a value.** A panic of the model is caught for each seed and
  becomes a `SeedFailure`. `run` and `report` panic with it; `try_run`
  returns it. The crate prints nothing itself; the panic hook of the
  process still prints each caught panic.
- `Walk` reads the environment only through `plan_from`, which the unit
  tests call with a closure. Do not call `std::env::set_var` in a test.
- Book: `book/src/part5-building-on-top/09-walk.md`.
