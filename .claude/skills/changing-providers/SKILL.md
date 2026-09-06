---
name: changing-providers
description: Add or change a moonpool provider trait method or a whole provider - the AFIT trait in moonpool-core (Send futures, Clone + Send + Sync + 'static), the Tokio implementation behind its granular feature in the same file, the sim implementation under crates/moonpool-sim/src/providers or the network/storage engines, the Providers bundle macro, the select! passthrough vs deterministic expansion, and the wasm / lean-tree / book gates. Use when touching TimeProvider, TaskProvider, NetworkProvider, RandomProvider, StorageProvider, block devices, or moonpool-core features.
---

# Changing providers

A provider method exists three times: the trait, the tokio implementation
and the sim implementation. The trait is the contract consumers write
production code against, so a change here is an API change with a book
chapter and a compatibility checklist attached.

## The three places

| Layer | Where | Rules |
|---|---|---|
| trait | `crates/moonpool-core/src/{time,task,network,random,storage,block}.rs` | native AFIT: `fn f(&self, ..) -> impl Future<Output = ..> + Send`; supertraits `Clone + Send + Sync + 'static`; associated types for streams and files (`futures::io`, never `tokio::io`) |
| tokio impl | the same file, `#[cfg(feature = "tokio-<slot>")]` (`TokioTimeProvider`, `TokioTaskProvider`, `TokioNetworkProvider`, `TokioRandomProvider`, `TokioStorageProvider`, `TokioBlockDeviceProvider` in `block_tokio.rs`); `TokioProviders` in `providers.rs` | plain `async fn`; each slot has its own feature so a consumer can take one without net/fs/time |
| sim impl | `crates/moonpool-sim/src/providers/{time,task,random}.rs`, `network/sim/provider.rs`, `storage/provider.rs`, `storage/block/`; `SimProviders` in `providers/sim_providers.rs` | every random choice from `SIM_RNG`; every wait through the `Scheduler`; storage and network completions as scheduled events, never inline |
| bundle | `impl_providers_bundle!` in `providers.rs` | `Providers` has one associated type and accessor per slot; extend the macro, not the two structs by hand |

`SimContext` (`runner/context.rs`) exposes the same accessors to processes
and workloads; add the accessor there when you add a slot.

## `select!`

`moonpool_core::select!` (`select.rs`, `select_support.rs`) is tokio's macro
verbatim under the `select` feature and tokio's expansion driven by a seeded
start offset under `deterministic-select`, which moonpool-sim enables and
routes through `install_select_offset` per iteration. Changing the macro means
keeping both shapes and the `moonpool-core --features select` test job green.

## Steps

1. Design the trait method so the sim can honour it deterministically. If it
   needs a wall clock or an OS handle, it is a different method than you
   think (`time.now()` is the scheduling clock, `time.timer()` the drifted
   application clock; keep that split).
2. Implement tokio and sim together in one change; a trait method with one
   implementation does not compile for the other bundle.
3. Features: the all-off `moonpool-core` build stays wasm-clean; the lean
   production tree (`moonpool --no-default-features --features tokio`) stays
   free of sim, explorer, hyper and prometheus; a new dependency goes behind
   the granular feature that needs it.
4. Tests: the provider's file under `crates/moonpool-sim/tests/` driven with
   the `drive` helper; `tests/conformance.rs` compares sim and tokio behaviour
   where both can run.
5. Book: `part2-foundations/07-provider-traits.md` (the full definitions),
   `04..06` if the rationale moved, `appendix/06-sim-compatibility.md` (the
   checklist consumers grep), `part4-integration/05-production.md`; and
   `book/src/llms.md` if the API anchor changed.
6. Gates (`/validate`): the portability job in full.

`moonpool-hyper` (`HyperIo`, `HyperExecutor`, `HyperTimer`) adapts these
traits for hyper; a stream or task trait change usually needs a matching
change there and in `tests/hyper_http.rs`.
