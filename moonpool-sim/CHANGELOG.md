# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0] - 2026-06-24

### 🚀 Features

- **moonpool-sim**: Correlate disk stall/throttle episodes per process ([#147](https://github.com/PierreZ/moonpool/pull/147))
- **moonpool-sim**: Dynamic disk stall/throttle episodes ([#126](https://github.com/PierreZ/moonpool/pull/126))
- **moonpool-sim**: Consolidate stop conditions into UntilCoverageStable ([#135](https://github.com/PierreZ/moonpool/pull/135))
- **moonpool-sim**: Buggify-driven config-knob value-perturbation ([#125](https://github.com/PierreZ/moonpool/pull/125))
- **moonpool-sim**: Topology/locality failure domains ([#121](https://github.com/PierreZ/moonpool/pull/121))
- **moonpool-sim**: Long-tail latency distribution (exponential/bimodal) for network + storage ([#67](https://github.com/PierreZ/moonpool/pull/67))
- **moonpool-sim**: Partial read delivery in simulated connections ([#66](https://github.com/PierreZ/moonpool/pull/66))
- **moonpool-sim**: Declarative enable_chaos with per-surface swarm/random modes ([#130](https://github.com/PierreZ/moonpool/pull/130))
- **moonpool-sim**: Swarm testing — workload operation-alphabet subsets
- **moonpool-sim**: Per-pair permanent latency (FDB SimClogging)
- **moonpool-sim**: Swarm testing — per-seed fault-family subsets (core)

### 🚜 Refactor

- **sim**: Extract sim-upgrade helper in SimStorageProvider
- **sim**: Build peer lists in a single pass
- **moonpool-sim**: Make explorer optional and compile the sim to wasm
- **moonpool-sim**: Cross-validate invariants with plain tracing events

### 🧪 Testing

- **moonpool-sim**: Add Tokio provider conformance suite + migration guide


## [0.7.0] - 2026-06-05

### 🚀 Features

- **sim**: Add CoveragePlateau iteration control
- **core**: Make moonpool-core wasm-buildable and abstract TaskProvider JoinHandle
- **sim**: Add SimTime formatter backed by SimulationLayerHandle

### 🐛 Bug Fixes

- **orchestrator**: Deterministic run-phase virtual-time budget to catch self-perpetuating timers
- **sim**: Prevent flaky capture loss in parallel tests
- **rebase**: Reconcile with main's futures::io and CoveragePlateau

### 🚜 Refactor

- **sim**: Plain-tracing capture + rename timelines to trails
- **sim**: Inline trivial run_simulation wrapper
- **core,sim**: Collapse provider getter forwards via macro
- **sim**: Extract init_sim_tracing helper for example binaries
- **core,sim,transport,derive**: Post-migration cleanup of Send-bounded traits
- **core,sim**: Switch trait impls back to async fn syntax
- **sim**: Swap Rc<RefCell<SimInner>> for Arc<RwLock<SimInner>>, spawn_local for spawn
- **sim**: Drop ?Send from Process/Workload/FaultInjector, switch to build()
- **sim**: Drop unused FaultContext::all_ips
- **sim**: Drop duplicate network/sim/types.rs ConnectionState/ListenerState
- **sim**: Drop dead AcceptFuture/SimTcpListener listener_id fields
- **sim**: Drop unused WakerRegistry::connection_wakers
- **sim**: Drop unused EventQueue::peek_earliest
- **sim**: Drop dead ConnectionState/ListenerState fields
- **sim**: Drop unused wake_send_buffer_waiters wrapper
- **sim**: Drop test-only SimulationLayerHandle::snapshot_event_count
- **sim**: Drop misc unused helpers across chaos/observability/topology/tags/storage
- **sim**: Drop unused InMemoryStorage helpers
- **sim**: Drop unused SimulationReport::check, is_success, and ReportCheckError
- **sim**: Drop unused network config helpers and chaos fields
- **sim**: Drop dead SimWorld connection helpers
- **sim**: Drop unused SimulationBuilder helpers
- **core**: Switch StorageFile IO bounds to futures::io
- **core**: Switch NetworkProvider IO bounds to futures::io
- **sim**: Introduce Clock trait, decouple SimTime from layer handle
- **sim**: Replace Timeline + Invariant with tracing-based SimulationLayer

### 🎨 Styling

- Fix clippy warnings for Rust 1.92

### 🧪 Testing

- **workspace**: Migrate tests/examples to Send-bounded traits

### ⚙️ Miscellaneous Tasks

- **workspace**: Enable clippy pedantic and fix all warnings
- **sim**: Drop stale lib.rs doc examples
- Bump Rust toolchain to 1.95.0

### 📦 Other

- Merge pull request #113 from FlorentinDUBOIS/feat/poll-write-vectored


## [0.6.0] - 2026-03-28

### 🚀 Features

- **moonpool-sim**: Add progress milestones and slow seed warnings
- **moonpool-transport**: Add 24 assertions, 2 buggify points, fix invariant reset and queue metrics
- **moonpool-sim**: Emit fault events to timeline from all injection sites
- **moonpool-sim**: Add event timeline API for temporal invariants
- **moonpool-sim**: Per-process storage fault injection ([#70](https://github.com/PierreZ/moonpool/pull/70))

### 🐛 Bug Fixes

- **moonpool-sim, moonpool-transport**: Replace expect()/unwrap() with Result in library code ([#56](https://github.com/PierreZ/moonpool/pull/56))
- Replace Result<T, String> with domain error enums (C-GOOD-ERR) ([#58](https://github.com/PierreZ/moonpool/pull/58))
- **moonpool-sim**: Replace string-based error matching with StorageError type ([#63](https://github.com/PierreZ/moonpool/pull/63))
- Rename get_ prefixed getters to follow C-GETTER convention

### 🚜 Refactor

- **moonpool**: Remove virtual actor system entirely
- **moonpool-sim**: Rework workload lifecycle with event-loop integration ([#52](https://github.com/PierreZ/moonpool/pull/52))

### 🧪 Testing

- **moonpool**: Add TransportTimelineCheck and per-message timeline events


## [0.5.0] - 2026-03-09

### 🚀 Features

- **sim**: Add details parameter to assertion macros for debug context
- **sim**: Add before_iteration hook and clear methods for multi-seed reset
- **sim**: Show per-seed timeline count in Seeds report section
- **sim,transport**: Replace assert_sometimes!(true) with correct assertion macros
- **moonpool-sim**: Add Process trait and reboot mechanics
- **moonpool-sim**: Add colored terminal display for simulation reports
- Extract moonpool-sim-examples crate for focused sancov coverage
- Add cargo xtask sim for running simulation binaries with sancov
- Move simulation tests to per-crate binary targets for sancov compatibility
- **moonpool-explorer**: Add sancov edge coverage to stats and reporting
- **moonpool-sim**: Add ClientId strategy for workload identity assignment

### 🐛 Bug Fixes

- **sim**: Create fresh tokio runtime per iteration for determinism
- **sim,explorer**: Fix TOCTOU race causing duplicate assertion slots
- **sim,explorer**: Fix duplicate assertion slots and false "was never reached" violations
- **moonpool-sim**: Implement graceful reboot, max_dead enforcement, and review fixes
- **moonpool-sim**: Remove redundant ERROR logs from always-type assertion macros

### 🚜 Refactor

- **moonpool-transport**: Migrate server workloads to Process trait

### ⚙️ Miscellaneous Tasks

- Checkpoint to non-deterministic bug


## [0.4.0] - 2026-02-19

### 🚀 Features

- **moonpool**: Replace control-plane test with AWS DynamoDB metastable failure simulation
- **moonpool-sim**: Enrich simulation report with detailed assertions, buckets, and per-seed metrics
- **moonpool-sim**: Add control-plane RPC exploration test
- **moonpool-explorer**: Add coverage-preserving multi-seed exploration
- **moonpool-explorer**: Add multi-core parallel exploration via sliding window fork loop
- **moonpool**: Backport Antithesis assertion suite — 14 macros, rich shared-memory slots
- **moonpool**: Complete Phases 7-10 — exploration tests, fork points, transport + actor workloads
- **moonpool-sim**: Rewrite builder + orchestrator with Workload lifecycle and two-phase chaos/recovery
- **moonpool-sim**: Add SimContext, Workload, FaultInjector, StateHandle, Invariant types
- **moonpool-sim**: Add assert_always!, assert_sometimes!, assert_sometimes_each! macros
- **moonpool-explorer**: Add adaptive forking, 3-level energy budgets, and exploration report
- **moonpool-explorer**: Add fork-based multiverse exploration crate
- **moonpool-sim**: Add RNG call counting and breakpoints
- **sim**: Add hyper HTTP/1.1 example for moonpool-sim
- **sim**: Integrate SimStorageProvider into SimProviders bundle
- **sim**: Implement SimStorageFile async I/O operations
- **sim**: Add SimStorageProvider wrapping WeakSimWorld
- **sim**: Integrate storage simulation into SimWorld
- **sim**: Add StorageOperation events for simulation
- **sim**: Add InMemoryStorage with deterministic fault injection
- **sim**: Add StorageConfiguration for storage simulation
- **core**: Add StorageProvider and StorageFile traits
- **sim**: Add FDB-aligned chaos features and comprehensive documentation

### 🐛 Bug Fixes

- **moonpool**: Fix metastable replay determinism with seeded tokio runtime
- **moonpool-sim**: Decouple assertion violations from workload errors
- **moonpool-sim**: Resolve deadlock in orchestrator when chaos disrupts connect
- **moonpool-explorer**: Set coverage bitmap in assert_sometimes_each, tune exploration configs
- **moonpool-sim**: Exploration tests were silently skipping assertions
- **moonpool-sim**: Trigger shutdown on any workload completion
- **sim**: Implement TCP half-close (FIN) semantics for graceful connection close
- **sim**: Update imports for rand 0.10 API changes

### 📚 Documentation

- Align documentation with current codebase state
- **moonpool-explorer**: Comprehensive rustdoc with glossary, ASCII diagrams, and walkthrough
- Update all READMEs, CLAUDE.md, and rustdoc for 6-crate architecture
- **sim**: Add storage testing patterns and complete doc coverage

### ⚡ Performance

- **moonpool-sim**: Reduce dungeon exploration to HalfCores, update CLAUDE.md

### 🚜 Refactor

- **moonpool-sim**: Use assert_always! for bug reporting in maze and dungeon
- **moonpool-sim**: Make always-assertions non-panicking (Antithesis style)
- **moonpool-sim**: Remove workload_fn, add WorkloadCount, clean up TODOs
- Unify test naming with slow_simulation_ prefix and nextest profiles
- **moonpool**: Port assertion macros, delete StateRegistry/InvariantCheck
- **moonpool**: Comment out old simulation tests for DX redesign
- **core**: Replace SimulationResult with TimeError in TimeProvider
- **sim**: Simplify storage simulation code
- **sim**: Extract storage operations from world.rs

### 🧪 Testing

- **moonpool-sim**: Add dungeon bug replay test
- **moonpool-sim**: Add maze bug replay test
- **sim**: Expand storage simulation test coverage
- **sim**: Add comprehensive storage simulation tests


## [0.3.0] - 2026-01-25

### 🚜 Refactor

- **transport**: Consolidate provider type params into Providers bundle


## [0.2.2] - 2025-12-18

### 🐛 Bug Fixes

- **docs**: Use correct docs.rs metadata keys for tokio_unstable

### 🚜 Refactor

- **sim**: Extract ConnectionReset error helpers in stream.rs
- **sim**: Extract sim_shutdown_error helper in stream.rs
- **sim,transport**: Remove unused dead code
- **sim**: Simplify MetricsCollector with focused helper methods
- **sim**: Extract event processing into focused handler methods
- **sim**: Remove redundant close_connection_graceful method
- **sim**: Reduce WeakSimWorld boilerplate with weak_forward! macro
- **sim**: Consolidate SimWorld constructors into single create() method


## [0.2.1] - 2025-12-04

### 🐛 Bug Fixes

- **docs**: Add docs.rs config and README for each crate


## [0.2.0] - 2025-12-03

### 🐛 Bug Fixes

- Resolve all clippy warnings and rustdoc issues

### 📚 Documentation

- Consolidate markdown docs into Rust doc comments

### 🚜 Refactor

- Reorganize into 4-crate architecture

