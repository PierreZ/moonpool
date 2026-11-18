# moonpool-walk

A seeded random walk over sans-I/O state machines: the judge between a model
checker and a `moonpool-sim` simulation. No executor, no network, no clock.

## Surface

| Item | Role |
|---|---|
| `Model` | what a user writes: `new`, `deliver`, and optionally `actions`/`act`, `nodes`/`is_up`/`crash`/`restart`, `calm`, `check`, `converged`, `describe` |
| `Cx` | what a model reaches while it is called: `rng`, `send`, `record`, `phase`, `step`, `seed` |
| `Walk` | the seeded schedule (chaos, quiet, settle) and the campaign: `run`, `report`, `try_run`, `run_each`, `replay`, `check_determinism` |
| `Harness` | the step engine, public for scripted schedules; `Walk` drives the same one |
| `Run`, `Logged`, `Entry`, `Stats` | everything one seed did |
| `Report`, `WalkError`, `SeedFailure`, `Site` | the result of a campaign |
| `Rng`, `Mailbox` | splitmix64 and the bounded bag of messages in flight |

## Rules

- **Dependencies: `moonpool-assertions` and `moonpool-buggify` only.** Both
  have zero dependencies. The crate must compile on
  `wasm32-unknown-unknown`. It must never depend on `moonpool-sim`,
  `moonpool-core` or `moonpool-tla`: a trace checker reads `Run::events`,
  the walk does not know it.
- **One generator for each run.** The schedule and the model draw from
  `Core::rng`. Buggify draws from its own stream (`seed ^ BUGGIFY_SALT`) and
  the swarm from another (`seed ^ SWARM_SALT`), so switching either on does
  not move the schedule of a seed.
- **Draw counts are a contract.** A change to the number or the order of the
  draws in `Walk::drive` moves every seed of every user. Say so in the PR.
- **The model holds the truth about who is up** (`Model::is_up`). The walk
  keeps no list of dead nodes: a node may crash by itself inside `deliver`.
- **The walk owns thread-local state only for the length of a campaign**
  (`Installed`): the assertion table when none was installed, and buggify
  when no simulation was running. It removes both on drop.
- **A failure is a value.** A panic of the model is caught for each seed and
  becomes a `SeedFailure`. `run` and `report` panic with it; `try_run`
  returns it. Do not print from the crate.
- `Walk` reads the environment only through `plan_from`, which the unit
  tests call with a closure. Do not call `std::env::set_var` in a test.
- Book: `book/src/part5-building-on-top/09-walk.md`.
