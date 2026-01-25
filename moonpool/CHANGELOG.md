# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


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

