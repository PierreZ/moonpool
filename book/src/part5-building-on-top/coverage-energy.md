# Coverage and Energy Budgets

<!-- toc -->

- Energy as finite exploration resource: prevents unbounded forking
- Three-level budget: global energy, per-mark energy, reallocation pool
- Global energy: total timelines allowed across the entire exploration
- Per-mark energy: budget allocated to each splitpoint/assertion
- Reallocation pool: energy returned from barren marks, redistributed to productive ones
- `ExplorationConfig`: `max_depth`, `global_energy`, `timelines_per_split`
- Coverage bitmap saturation: 8192 bits can saturate with many unique paths — affects when marks appear "barren"
