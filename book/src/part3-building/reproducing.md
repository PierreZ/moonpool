# Reproducing with FixedCount

<!-- toc -->

- `SimulationBuilder::set_seed(failing_seed)` + `FixedCount(1)`
- Set `ERROR` log level for maximum detail
- Same seed = same execution = same bug, every time
- If the bug disappears: check for non-determinism (direct tokio calls, system random, etc.)
