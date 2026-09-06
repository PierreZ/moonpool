---
name: changing-assertion-accounting
description: Change how moonpool counts assertions and fault injection - the fifteen assert_* macros in crates/moonpool-sim/src/chaos/assertions.rs, the slot and bucket tables in moonpool-assertions (512 slots, 64-byte message hash identity, 256 buckets, 6 keys), validate_assertion_contracts and the report's violation lists, the explorer's shared-memory overlay and discovery hook, and the buggify two-phase state in moonpool-buggify plus buggify_knob! in moonpool-sim. Use when adding a macro, changing a budget, changing what counts as a violation, or touching buggify activation.
---

# Changing assertion accounting

The assertion tables are a **wire format**. A slot's identity is the hash of
its message truncated to 64 bytes, saturation history downstream (paros's
sweep, anyone's `until_coverage_stable`) is keyed on it, and the explorer
shares the same region across forked workers. Treat a change here the way
you would treat a change to a serialized struct.

## Map

| Piece | Where | Notes |
|---|---|---|
| the macros (15) | `crates/moonpool-sim/src/chaos/assertions.rs` | `assert_always!` (+ detail-map form), `assert_always_or_unreachable!`, `assert_sometimes!`, `assert_reachable!`, `assert_unreachable!`, four `assert_always_<cmp>!`, four `assert_sometimes_<cmp>!`, `assert_sometimes_all!`, `assert_sometimes_each!`. None panic: always-failures call `record_always_violation()` and the run continues |
| accounting | `crates/moonpool-assertions/src/{slots,buckets,region,..}.rs` | pure std, zero deps, wasm-able; `MAX_ASSERTION_SLOTS = 512`, `SLOT_MSG_LEN = 64`, `MAX_EACH_BUCKETS = 256`, `MAX_EACH_KEYS = 6`; heap table by default, `install_region` overlays `MAP_SHARED` memory for the explorer |
| contracts | `chaos::validate_assertion_contracts() -> (always_violations, coverage_violations)`, `has_always_violations`, `reset_always_violations`, `reset_assertion_results` | a seed fails on `Err` or an always violation; coverage violations fail the campaign; slot overflow is counted (`dropped_assertion_allocations`) **and** reported as an always violation |
| report | `runner/report.rs` (`assertion_violations`, `coverage_violations`, `assertion_results`, `assertion_details`, `bucket_summaries`, `saturation`), printed by `runner/display.rs` | |
| exploration glue | `chaos/exploration_glue.rs` | the discovery hook the explorer installs: first success of a sometimes, a new bucket, a frontier or watermark improvement forks |
| buggify | `crates/moonpool-buggify/src/lib.rs` (`buggify!`, `buggify_with_prob!`, the two-phase state, inert without a source), `crates/moonpool-sim/src/chaos/buggify.rs` (`buggify_init(0.5)` per iteration, `buggify_reset`, `buggify_knob!`) | activation 0.5 per location per run, firing 0.25 or the caller's rate |

## Rules

- **Do not change the hash or the truncation** without accepting that every
  consumer's saturation history resets; if you must, say so in the changelog
  and the book.
- **Budgets are shared** with moonpool's own internal assertions; raising a
  budget changes the explorer's shared region size (`ASSERTION_TABLE_MEM_SIZE`)
  and must keep the wasm build of `moonpool-assertions` and the
  `--no-default-features` sim (heap table) working.
- **A new macro** needs: the accounting kind in `moonpool-assertions`, the
  `AssertKind`/`AssertCmp` plumbing, contract validation semantics (does an
  unmet instance fail the seed, the campaign, or nothing), the explorer's
  discovery hook if its first success should fork, `report.eprint()` output,
  a test in `tests/chaos/`, and `appendix/01-assertion-reference.md` plus
  `part3-building/12..16` in the book.
- **Overflow stays loud.** Never let a table grow silently or drop silently.
- **Buggify stays inert without a source.** The macros must return `false`
  in a production binary; moonpool-sim is the only installer, once per
  iteration in `reset_per_iteration_state`. `buggify_knob!` stays in
  moonpool-sim because it needs the ranged draw.
- Every draw the accounting or buggify makes goes through `SIM_RNG`.

Tests: `tests/chaos.rs` + `tests/chaos/`, `tests/coverage_plateau.rs`,
`tests/exploration.rs`; then `cargo xtask sim run frontier-explore` (the
explorer's own scenarios exercise the shared region).
