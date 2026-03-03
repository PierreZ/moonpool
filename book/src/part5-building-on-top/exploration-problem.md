# The Exploration Problem

<!-- toc -->

- Why random simulation isn't enough: some bugs require a sequence of unlikely events
- Sequential luck problem: a bug requiring 1/1000 failover followed by 1/1000 rollback needs ~1M trials
- With checkpoint-and-branch: drops to ~1/2000
- State space shapes: breadth-dominated (many paths, shallow) vs depth-dominated (few paths, deep)
- Barriers and doomed states: some states are dead ends that waste exploration budget
- Inline NES analogies:
  - Zelda: open-world exploration, 25 sub-goals, equalized coverage across 358 rooms
  - Gradius: depth-dominated, simple strategy (survive) beats complex strategy
  - Castlevania: barriers (stompers as local optima), strategy vs tactics decomposition
  - Metroid: continuous optimization alongside exploration (missile economy)
- The key insight: exploration is a resource allocation problem, not just a randomness problem
