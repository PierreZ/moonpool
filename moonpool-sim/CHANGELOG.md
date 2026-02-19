# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

