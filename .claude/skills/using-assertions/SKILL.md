---
description: |
  Moonpool assertion macros: assert_always!, assert_sometimes!, assert_reachable!, numeric and compound assertions.
  TRIGGER when: writing or modifying assert_always!, assert_sometimes!, assert_reachable!, assert_unreachable!, or any moonpool assertion macro. Also when choosing between assertion types or adding coverage tracking.
  DO NOT TRIGGER when: using standard Rust assert!/assert_eq! or non-moonpool testing.
---

# Using Assertions

## When to Use This Skill

Invoke when:
- Adding assertions to simulation workloads, invariant checks, or library code
- Choosing between `assert_always!` and `assert_sometimes!` families
- Tracking coverage of error paths, chaos injection, or probabilistic scenarios
- Validating safety invariants that must never be violated
- Using numeric comparisons to guide exploration toward boundary bugs
- Tracking multi-step workflows or per-value exploration buckets

## Quick Reference

| Macro | Panics? | Forks? | Use for |
|-------|---------|--------|---------|
| `assert_always!(cond, msg)` | yes | no | Invariants that must always hold |
| `assert_always_or_unreachable!(cond, msg)` | yes | no | Invariants on optional code paths |
| `assert_sometimes!(cond, msg)` | no | on first success | Coverage targets (path tested at least once) |
| `assert_reachable!(msg)` | no | on first reach | Code path must be reachable |
| `assert_unreachable!(msg)` | yes | no | Code path must never execute |
| `assert_always_greater_than!(val, thresh, msg)` | yes | no | Numeric lower bound (strict) |
| `assert_always_greater_than_or_equal_to!(val, thresh, msg)` | yes | no | Numeric lower bound (inclusive) |
| `assert_always_less_than!(val, thresh, msg)` | yes | no | Numeric upper bound (strict) |
| `assert_always_less_than_or_equal_to!(val, thresh, msg)` | yes | no | Numeric upper bound (inclusive) |
| `assert_sometimes_greater_than!(val, thresh, msg)` | no | on watermark improvement | Value should sometimes exceed threshold |
| `assert_sometimes_greater_than_or_equal_to!(val, thresh, msg)` | no | on watermark improvement | Value should sometimes reach threshold |
| `assert_sometimes_less_than!(val, thresh, msg)` | no | on watermark improvement | Value should sometimes go below threshold |
| `assert_sometimes_less_than_or_equal_to!(val, thresh, msg)` | no | on watermark improvement | Value should sometimes reach threshold |
| `assert_sometimes_all!(msg, [(name, val), ...])` | no | on frontier advance | All named bools should eventually be true simultaneously |
| `assert_sometimes_each!(msg, [(name, val), ...])` | no | on new bucket | Per-value bucketed exploration |

All macros accept `&str` or `String` for message arguments. Macros are exported from `moonpool_sim`.

## Book Chapters

Read these for detailed usage, decision flowcharts, examples, best practices, and validation contracts:

- `book/src/part3-building/12-assertions.md` — overview and assertion taxonomy
- `book/src/part3-building/13-assertion-concepts.md` — invariants vs discovery vs guidance
- `book/src/part3-building/14-always-sometimes.md` — boolean assertions in depth
- `book/src/part3-building/15-numeric-assertions.md` — numeric assertions and watermark tracking
- `book/src/part3-building/16-compound-assertions.md` — `assert_sometimes_all!` and `assert_sometimes_each!`
- `book/src/part3-building/17-invariants.md` — system invariants
- `book/src/appendix/01-assertion-reference.md` — complete macro reference with examples

## Source

Macros defined in `moonpool-sim/src/chaos/assertions.rs`. Backing shared-memory infrastructure in `moonpool-explorer/src/assertion_slots.rs`.
