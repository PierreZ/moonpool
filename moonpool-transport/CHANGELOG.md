# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

