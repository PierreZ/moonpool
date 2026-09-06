# moonpool-assertions

Antithesis-style assertion accounting: the slot table, the per-bucket table
for `sometimes_each`, numeric watermarks and the contract validation that
turns them into `assertion_violations` / `coverage_violations`.

- **Pure std, zero dependencies, wasm-able.** `[dependencies]` stays empty;
  the crate is checked for `wasm32-unknown-unknown` in CI. Anything that needs
  libc, mmap or fork belongs in `moonpool-explorer`, which overlays a
  `MAP_SHARED` region and a discovery hook on top of the heap table this crate
  provides by default.
- **Budgets are constants here and a contract everywhere**:
  `MAX_ASSERTION_SLOTS = 512` (`slots.rs`), message truncation at
  `SLOT_MSG_LEN = 64`, `MAX_EACH_BUCKETS = 256` and `MAX_EACH_KEYS = 6`
  (`buckets.rs`). A slot's identity is the hash of its message; changing how
  the hash or the truncation works resets every campaign's saturation
  history downstream, so treat it as a wire format.
- Overflow must stay loud: a dropped allocation is counted
  (`dropped_assertion_allocations`) and reported as an always-violation by
  moonpool-sim. Never make the table silently grow or silently drop.
- The macros that call into this crate live in
  `crates/moonpool-sim/src/chaos/assertions.rs`; their reference chapter is
  `book/src/appendix/01-assertion-reference.md`.
