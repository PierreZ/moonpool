# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

