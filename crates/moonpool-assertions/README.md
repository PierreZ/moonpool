# moonpool-assertions

Portable, Antithesis-style assertion accounting for the moonpool framework.

## What it counts

- **Boolean assertions**: always, always-or-unreachable, sometimes, reachable,
  unreachable.
- **Numeric guidance**: comparisons that also keep a best-value watermark, so
  exploration can steer toward states near a boundary.
- **Compound `sometimes_all`**: a set of propositions that should all hold at
  once, with a frontier of how many held together so far.
- **Per-value `sometimes_each` buckets**: one bucket per distinct combination of
  identity keys, with an optional quality watermark.

`moonpool-sim` turns the counts into a report's `assertion_violations`
(definite bugs) and `coverage_violations` (paths a campaign never reached).

## Portable by design

The crate has **zero dependencies** and compiles for `wasm32-unknown-unknown`,
macOS and Linux. By default the counts live in a heap-allocated table in one
process. `moonpool-explorer` installs `MAP_SHARED` memory and a discovery hook
over the same accounting, so forked exploration timelines add to one shared
table.

## Budgets

The tables are fixed-size: 2048 assertion sites and 256 `sometimes_each`
buckets (at most 6 identity keys each). A site's identity is the hash of its
message. An evaluation that finds a table full is counted, and the simulation
reports the overflow as an always-violation, so running out of room is never
silent.

## Documentation

- [API Documentation](https://docs.rs/moonpool-assertions)
- [Assertion reference](https://pierrez.github.io/moonpool/appendix/01-assertion-reference.html)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
