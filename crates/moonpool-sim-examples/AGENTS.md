# moonpool-sim-examples

Runnable simulations that double as the book's worked examples and as CI's
end-to-end gate. Each is a library module (`src/<name>.rs`) with a thin binary
under `src/bin/sim/<name>.rs` that wires the builder, prints the report and
exits non-zero when `seeds_failing` is non-empty.

| Module | Shows |
|---|---|
| `maze.rs` | frontier exploration through nested low-probability gates |
| `dungeon.rs` | forked-worker exploration and the reference operation-alphabet swarm (`swarm_enabled_actions`, `pick_action_index`) |
| `axum_web.rs` | an unmodified axum/hyper HTTP/1 service over simulated TCP via `moonpool_hyper::HyperIo` |
| `metrics_service.rs` | application metrics through `.metrics_factory()` / `.metric()` |
| `topology.rs` | failure domains with `.cluster(LocalityConfig, ..)` and machine-scoped attrition |
| `tonic_grpc.rs` | a tonic service over hyper HTTP/2 under network chaos plus `Chaos::Attrition` |

## Adding an example

Three registrations, in one change, or CI never runs it:

1. the module and its `[[bin]]` in this crate (`src/bin/sim/<name>.rs`, named
   `sim-<name>`);
2. `SIM_BINARIES` in `crates/xtask/src/main.rs` (the sancov whitelist names the
   crate with underscores);
3. the `sim` matrix in `.github/workflows/rust.yml`.

Verbosity is `init_sim_tracing(Level)` in `main`; seeds and iteration counts
are builder calls, not CLI flags (`cargo xtask sim run <name>` passes anything
after `--` to the binary, but no example parses arguments today). An example
should be reference-quality: factory-created workloads, every assertion
message stable, a `sometimes` for each outcome it claims to reach, and a
comment above every buggify site saying why that boundary.
