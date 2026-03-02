# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [0.5.0] - 2026-03-02

### 🚀 Features

- **moonpool**: Add structured tracing across actor runtime
- **moonpool**: Wire placement to membership active members
- **moonpool**: Add node self-registration and shutdown cleanup
- **moonpool**: Evolve ActorDirectory with Orleans-style semantics
- **moonpool**: Add ActivationId and ActorAddress types
- **moonpool**: Evolve MembershipProvider with snapshot and registration
- **moonpool**: Add core membership types for Orleans parity
- **moonpool**: Pass ActorContext to virtual actor trait methods, add NodeConfig and ClusterConfig builder
- **moonpool**: Add MoonpoolNode unified actor runtime
- **moonpool-sim**: Add colored terminal display for simulation reports
- Add cargo xtask sim for running simulation binaries with sancov
- Move simulation tests to per-crate binary targets for sancov compatibility

### 🚜 Refactor

- **moonpool**: Move directory registration from caller to target host
- **moonpool**: Separate PlacementStrategy enum from PlacementDirector trait
- **moonpool**: Reorganize actors/ into folder-based sub-modules
- Remove nextest fast profile, add xtask sim subcommands


## [0.4.0] - 2026-02-19

### 🚀 Features

- **moonpool**: Replace control-plane test with AWS DynamoDB metastable failure simulation
- **moonpool-transport-derive**: Unified #[service] macro, #[actor_impl], and serve()
- **moonpool**: Complete Phases 7-10 — exploration tests, fork points, transport + actor workloads
- **moonpool-sim**: Add RNG call counting and breakpoints
- **moonpool**: Add graceful stop for ActorHost
- **moonpool**: Add DeactivateAfterIdle idle timeout for actors
- **moonpool**: Add per-identity concurrent actor processing
- **moonpool**: Add Orleans-style actor lifecycle and state persistence
- **moonpool**: Add multi-node virtual actors with forwarding and simulation tests
- **moonpool-transport-derive**: Add #[virtual_actor] macro: generates ActorRef, dispatcher, and method constants
- **moonpool**: Add ActorHost: server-side runtime with automatic actor activation and dispatch
- **moonpool**: Add banking example with virtual actors and static endpoints
- **moonpool**: Add ActorRouter for caller-side actor request resolution
- **moonpool**: Add PlacementStrategy trait and LocalPlacement
- **moonpool**: Add ActorDirectory trait and InMemoryDirectory implementation
- **moonpool**: Add core virtual actor types (ActorId, ActorMessage, ActorResponse)

### 🐛 Bug Fixes

- **moonpool**: Fix metastable replay determinism with seeded tokio runtime
- **moonpool**: Fix metastable replay test assertion key and add fast replay test

### 📚 Documentation

- Align documentation with current codebase state
- Update all READMEs, CLAUDE.md, and rustdoc for 6-crate architecture

### 🎨 Styling

- **moonpool**: Apply cargo fmt formatting


## [0.2.2] - 2025-12-18

### 🐛 Bug Fixes

- **docs**: Use correct docs.rs metadata keys for tokio_unstable

### 🚜 Refactor

- **transport**: Rename FlowTransport to NetTransport


## [0.2.1] - 2025-12-04

### 🐛 Bug Fixes

- **docs**: Add docs.rs config and README for each crate


## [0.2.0] - 2025-12-03

### 🚀 Features

- **moonpool**: Implement Phase 12D testing improvements
- **moonpool**: Implement Phase 12C developer experience improvements
- **moonpool**: Add real TCP ping-pong example demonstrating RPC
- **moonpool**: Complete multi-node RPC transport layer (Phase 12B Step 7d)
- **moonpool**: Add multi-node RPC simulation infrastructure (Phase 12B Step 7d)
- **moonpool**: Add multi-node transport with take_receiver() pattern
- **moonpool**: Add RPC simulation tests with FDB-style invariants (Phase 12B Step 10)
- **moonpool**: Add RPC integration tests (Phase 12B Step 9)
- **moonpool**: Implement FDB-style request-response RPC (Phase 12B)
- **moonpool-traits**: Add pluggable MessageCodec for serialization
- **moonpool**: Add simulation tests and sometimes_assert! coverage (Phase 12 Step 7a)
- **moonpool**: Implement FDB-style static messaging core (Phase 12)
- Implement Phase 2a network traits and Tokio integration

### 🐛 Bug Fixes

- **moonpool**: Add shutdown mechanism to listen_task preventing timeout

### 📚 Documentation

- Consolidate markdown docs into Rust doc comments

### 🚜 Refactor

- Reorganize into 4-crate architecture

