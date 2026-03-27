---
description: |
  Moonpool fault injection: buggify!() and buggify_with_prob!() macros for chaos testing.
  TRIGGER when: adding fault injection points, using buggify!() or buggify_with_prob!(), or deciding where to inject chaos in Process code.
  DO NOT TRIGGER when: not working on moonpool simulation code.
---

# Using Buggify

## When to Use This Skill

Invoke when:
- Adding fault injection points to code
- Choosing between `buggify!()` and `buggify_with_prob!()`
- Deciding where to place chaos injection in a code path
- Calibrating injection probabilities

## Quick Reference

```rust
// Default: 25% firing probability (when activated)
if buggify!() {
    return Err(Error::timeout("simulated timeout"));
}

// Custom probability
if buggify_with_prob!(0.5) {
    buffer_size = 1;
}
```

**Two-phase activation**: First encounter → 50% chance of activation (per seed). Subsequent encounters at active site → 25% firing probability.

**Five injection patterns**:
1. Error injection on success paths — `if result.is_ok() && buggify!() { return Err(...) }`
2. Artificial delays — `if buggify!() { time.sleep(...).await; }`
3. Parameter randomization — `let size = if buggify!() { 1 } else { DEFAULT };`
4. Alternative code paths — `let compact = needs_compaction() || buggify_with_prob!(0.1);`
5. Process restarts — `if buggify_with_prob!(0.01) { return Err(Error::crash(...)); }`

**Probability tiers**: High (5-10%) for common edges, medium (1%) for standard faults, low (0.1-0.01%) for rare critical failures.

**Production safe**: `buggify!()` always returns `false` outside simulation.

## Book Chapters

- `book/src/part3-building/08-buggify.md` — full guide: two-phase activation, five patterns, calibration, anti-patterns

## Source

Macros defined in `moonpool-sim/src/chaos/buggify.rs`.
