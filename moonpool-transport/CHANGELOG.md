# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.7.0] - 2026-05-22

### 🚀 Features

- **core**: Make moonpool-core wasm-buildable and abstract TaskProvider JoinHandle
- Add on_failed_for to failure monitor
- Serializable interfaces with endpoint adjustment
- **transport**: Add LoadBalanceConfig with exponential backoff
- **transport**: Add load_balance and fan-out primitives

### 🐛 Bug Fixes

- **rebase**: Reconcile with main's futures::io and CoveragePlateau
- Send EndpointNotFound notification back to client
- **transport**: Fast-path fan_out_quorum when required is zero

### 📚 Documentation

- Document broken_promise drop semantics
- **transport**: Add load_balance and fan-out examples

### 🚜 Refactor

- **transport**: Extract race_reply_or_signal helper in rpc::delivery
- **transport**: Dedupe test_address and reply_promise lock pattern
- **core,sim,transport,derive**: Post-migration cleanup of Send-bounded traits
- **core,sim**: Switch trait impls back to async fn syntax
- **core,transport,derive**: Migrate transport to Send-bounded traits
- **transport**: Drop dead PeerCore::providers field
- **transport**: Drop unused InterfaceMethod::try_recv_with_sender
- **transport**: Drop unused ReplyFuture::endpoint
- **transport**: Drop unused NetTransport::incoming_peer_count
- **transport**: Drop unused FailureMonitor::address_state
- **transport**: Drop unused smoother module
- **transport**: Drop unused MessagingError::QueueFull and TransportClosed
- **transport**: Drop unused TransportHandle::sleep
- **transport**: Drop unused ReplyPromise::is_fulfilled and endpoint
- **transport**: Drop unused NetNotifiedQueue::with_address and rand_simple_id
- **transport**: Drop unused InterfaceMethod accessors
- **transport**: Drop unused FailureMonitor::on_state_changed
- **transport**: Drop unused MessageReceiver::is_stream default method
- **transport**: Drop unused method_endpoint/method_uid helpers and interface.rs
- **transport**: Drop dead peer module APIs
- **core**: Switch NetworkProvider IO bounds to futures::io
- **transport**: Apply rust-api-guidelines review
- Examples use get_reply_unless_failed_for, demonstrate send()
- Unify Server and Client into single interface type
- Move codec to transport, erase from user-facing API
- Erase codec and provider generics from user-facing types
- Bind transport at interface construction
- Dynamic endpoint tokens with well-known opt-in

### ⚙️ Miscellaneous Tasks

- **workspace**: Enable clippy pedantic and fix all warnings
- **transport**: Drop unused futures, tokio-util, tracing-subscriber deps
- Remove simulation code from transport
- Remove load_balance and fan_out primitives


## [0.6.0] - 2026-03-28

### 🚀 Features

- **moonpool-transport**: Add 24 assertions, 2 buggify points, fix invariant reset and queue metrics
- **moonpool-transport**: Add reliable burst op and buggify queue size
- **moonpool-transport**: Add 18 assertions, 4 buggify points, fix failure monitor eviction
- **moonpool-transport**: Replace BoundClient with ServiceEndpoint

### 🐛 Bug Fixes

- **moonpool-transport**: Fix queue_metrics_consistent_after_failure drift
- **moonpool-transport**: Unregister reply endpoints on ReplyFuture drop
- **moonpool-sim, moonpool-transport**: Replace expect()/unwrap() with Result in library code ([#56](https://github.com/PierreZ/moonpool/pull/56))
- **moonpool-transport**: Close queue on deserialization failure instead of silent drop ([#61](https://github.com/PierreZ/moonpool/pull/61))
- Replace Result<T, String> with domain error enums (C-GOOD-ERR) ([#58](https://github.com/PierreZ/moonpool/pull/58))

### 🚜 Refactor

- **moonpool-transport**: Remove simulation suite for ground-up rewrite
- **moonpool-transport**: Embed codec in ServiceEndpoint
- **moonpool**: Remove virtual actor system entirely

### 🧪 Testing

- **moonpool**: Add TransportTimelineCheck and per-message timeline events


## [0.5.0] - 2026-03-09

### 🚀 Features

- **sim**: Make spacesim RPC fault-aware with try_get_reply
- **transport**: Add try_get_reply and send delivery modes
- **transport**: Add MaybeDelivered error and reply queue closure on disconnect
- **transport**: Add FailureMonitor for address/endpoint failure tracking
- **transport**: Add peer disconnect signal
- **sim,transport**: Replace assert_sometimes!(true) with correct assertion macros
- **moonpool**: Add MoonpoolNode unified actor runtime
- **moonpool-transport**: Add peer monitoring with ping/pong health detection
- **moonpool-sim**: Add colored terminal display for simulation reports
- Add cargo xtask sim for running simulation binaries with sancov
- Move simulation tests to per-crate binary targets for sancov compatibility

### 🐛 Bug Fixes

- **sim**: Create fresh tokio runtime per iteration for determinism
- **transport**: Self-notify connection task after write failure to enable reconnection
- **actors**: Add RPC timeout to prevent deadlock on connection death

### 🚜 Refactor

- **moonpool-transport**: Migrate server workloads to Process trait
- **moonpool**: Separate PlacementStrategy enum from PlacementDirector trait


## [0.4.0] - 2026-02-19

### 🚀 Features

- **moonpool-transport-derive**: Unified #[service] macro, #[actor_impl], and serve()
- **moonpool**: Complete Phases 7-10 — exploration tests, fork points, transport + actor workloads
- **moonpool**: Add multi-node virtual actors with forwarding and simulation tests
- **moonpool-transport-derive**: Add #[virtual_actor] macro: generates ActorRef, dispatcher, and method constants
- **transport-derive**: Reserve token index 0 for virtual actor dispatch
- **core**: Add StorageProvider and StorageFile traits

### 🐛 Bug Fixes

- **moonpool**: Fix metastable replay determinism with seeded tokio runtime
- **moonpool-sim**: Decouple assertion violations from workload errors

### 📚 Documentation

- Align documentation with current codebase state
- Update all READMEs, CLAUDE.md, and rustdoc for 6-crate architecture

### 🚜 Refactor

- **moonpool-sim**: Make always-assertions non-panicking (Antithesis style)
- **moonpool**: Port assertion macros, delete StateRegistry/InvariantCheck
- **moonpool**: Comment out old simulation tests for DX redesign
- **core**: Replace SimulationResult with TimeError in TimeProvider


## [0.3.0] - 2026-01-25

### 🚀 Features

- **transport**: Add Orleans-style ergonomic bound client pattern
- **transport**: Add #[derive(Interface)] macro for FDB-style interfaces

### 🚜 Refactor

- **transport**: Consolidate provider type params into Providers bundle
- **transport**: Improve code quality with 3 fixes
- **transport**: Change #[derive(Interface)] to #[interface] attribute macro

### ⚙️ Miscellaneous Tasks

- Remove false FDB-compatible protocol claims


## [0.2.2] - 2025-12-18

### 🐛 Bug Fixes

- **docs**: Use correct docs.rs metadata keys for tokio_unstable

### 🚜 Refactor

- **transport**: Add are_queues_empty helper to PeerSharedState
- **sim,transport**: Remove unused dead code
- **transport**: Extract serialize_message helper in Peer
- **transport**: Unify connection_task and incoming_connection_task
- **transport**: Rename FlowTransport to NetTransport


## [0.2.1] - 2025-12-04

### 🐛 Bug Fixes

- **docs**: Add docs.rs config and README for each crate


## [0.2.0] - 2025-12-03

### 🚀 Features

- **transport**: Restore ping_pong and calculator examples

### 🐛 Bug Fixes

- Resolve all clippy warnings and rustdoc issues

### 📚 Documentation

- Consolidate markdown docs into Rust doc comments

### 🚜 Refactor

- Reorganize into 4-crate architecture

