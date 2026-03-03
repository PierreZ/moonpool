# Configuration Reference

<!-- toc -->

- `SimulationBuilder` options: all methods with types and defaults
- `NetworkConfig`: latency range, packet loss rate, reordering, connection close probability
- `Attrition`: max_dead, probability weights, recovery delay, grace period
- `ExplorationConfig`: max_depth, global_energy, timelines_per_split, adaptive
- `AdaptiveConfig`: batch_size, min/max_timelines, per_mark_energy, warm_min_timelines
- `PeerConfig`: initial_backoff, max_backoff
- `IterationControl`: UntilAllSometimesReached(N), FixedCount(N)
