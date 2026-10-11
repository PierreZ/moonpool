# moonpool-assertions

Portable, Antithesis-style assertion accounting for the moonpool framework,
with zero dependencies.

This crate counts. It keeps a fixed-size table of assertion sites and a
table of per-value buckets, and it tells an exploration backend when
something globally new happens. The fifteen `assert_*!` macros that tests use
live in `moonpool-sim`, which records here and turns the counts into a
report.

## What it counts

| Accounting | `moonpool-sim` macros |
|------------|-----------------------|
| Boolean | `assert_always!`, `assert_always_or_unreachable!`, `assert_sometimes!`, `assert_reachable!`, `assert_unreachable!` |
| Numeric, with a best-value watermark | `assert_always_{greater,less}_than[_or_equal_to]!`, `assert_sometimes_{greater,less}_than[_or_equal_to]!` |
| Compound, with a frontier | `assert_sometimes_all!` |
| Per-value buckets, with a quality watermark | `assert_sometimes_each!` |

A failed **always** site is a bug: a report's `assertion_violations`. A
**sometimes** or **reachable** site that never passes is a coverage gap: a
report's `coverage_violations`. The `UntilCoverageStable` iteration control
stops when the sometimes sites have fired and coverage stops growing.

## Probes in shipped code

```rust
fn on_lease_lost() {
    moonpool_assertions::reachable!("lease lost while serving a read");
}

fn on_retry(retries: u32) {
    moonpool_assertions::sometimes!(retries > 2, "a request needed more than two retries");
}
```

`reachable!` (FoundationDB's `CODE_PROBE`) and `sometimes!` are for
production code. Outside a simulation no table is installed, and each call
costs one pointer read. A site's identity is the 64-bit FNV-1a hash of its
message, so a probe and a simulation's `assert_reachable!` with the same text
share one slot. Never reword a message.

## Portable by design

The crate compiles for `wasm32-unknown-unknown`, macOS and Linux. By default
the counts live in heap-allocated tables in one process. `moonpool-explorer`
installs `MAP_SHARED` memory with `install_region` and a discovery hook with
`set_discovery_hooks`, so forked exploration timelines add to one shared
table. Each first pass, watermark improvement, frontier advance and new
bucket reaches the hook exactly once across all processes.

## Budgets

The tables have a fixed size: 2048 assertion sites and 256 `sometimes_each`
buckets of at most six identity keys each. An evaluation that finds a table
full is counted. A site reached with a different kind, or a message that
collides with another, is flagged as conflicted. `moonpool-sim` reports both
as always-violations, so a lost evaluation is never silent.

## Documentation

- [Assertion reference](https://pierrez.github.io/moonpool/appendix/01-assertion-reference.html) in the Sim Book
- [API documentation](https://docs.rs/moonpool-assertions)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
