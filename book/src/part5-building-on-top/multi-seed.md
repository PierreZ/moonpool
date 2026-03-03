# Multi-Seed Exploration

<!-- toc -->

- Why: one seed with massive energy hits diminishing returns; multiple seeds explore different regions
- `prepare_next_seed(energy)`: selective reset preserving explored map + watermarks
- What's preserved across seeds: coverage bitmap union, assertion watermarks/frontiers, best scores
- What's reset: split triggers, pass/fail counts, energy budget, stats, bug recipe
- Warm starts: `warm_min_timelines` controls when marks on warm seeds are considered barren
- Result: 3 seeds × 400K energy runs faster and finds more than 1 seed × 2M energy
