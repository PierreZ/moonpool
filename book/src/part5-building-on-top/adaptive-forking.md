# Adaptive Forking

<!-- toc -->

- Problem: fixed timelines-per-split wastes energy on unproductive splitpoints
- Solution: batch-based forking with early termination
- `AdaptiveConfig`: `batch_size`, `min_timelines`, `max_timelines`, `per_mark_energy`
- Batch loop: fork a batch, check `has_new_bits()` BEFORE `merge_from()`, stop if barren
- Barren mark detection: if N batches produce no new coverage, return remaining energy to pool
- `min_timelines`: minimum exploration before declaring a mark barren
- `max_timelines`: cap even for productive marks
