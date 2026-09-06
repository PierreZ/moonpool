# moonpool-core

The provider traits and the core types every other crate (and every consumer's
production code) is written against. It must stay buildable for
`wasm32-unknown-unknown` with all features off, because that is the build the
browser demo and the no-default-features simulation use.

## Conventions that must hold in this crate

- **Provider traits are native AFIT**, not `async_trait`: `Clone + Send + Sync +
  'static` supertraits, `fn f(&self, ..) -> impl Future<Output = ..> + Send` in
  the trait so `Send` propagates through generic callers. Implementations
  write plain `async fn`. Do not add an object-safe wrapper here; erase at an
  application boundary with a narrow trait when a consumer needs `dyn`.
- **Streams are `futures::io`**, never `tokio::io`. hyper integration lives in
  `moonpool-hyper` (`HyperIo`), not here.
- **Every feature is granular and documented in `Cargo.toml`.** `tokio-providers`
  is the production umbrella over `tokio-task`/`-time`/`-net`/`-fs`/`-random`;
  `select` re-exports tokio's macro verbatim; `deterministic-select` (enabled
  by moonpool-sim) swaps in the seeded-offset expansion in `select.rs` +
  `select_support.rs`. A new feature must keep the all-off build wasm-clean and
  the lean production tree (`moonpool --no-default-features --features tokio`)
  free of sim, explorer, hyper and prometheus (CI checks both).
- `time.now()` is the scheduling clock and `time.timer()` the application
  clock that clock-skew faults drift; keep both, and document which one a new
  API reads.
- Public API changes here ripple into `book/src/part2-foundations/04..07-*.md`
  and `appendix/06-sim-compatibility.md` in the same change.

## Gates

```bash
nix develop --command cargo nextest run -p moonpool-core --features select
nix develop --command cargo check --target wasm32-unknown-unknown -p moonpool-assertions
nix develop --command cargo check --target wasm32-unknown-unknown -p moonpool-sim --no-default-features
```
