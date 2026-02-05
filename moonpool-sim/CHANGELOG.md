# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-02-05

### 🚀 Features

- **sim**: Integrate SimStorageProvider into SimProviders bundle
- **sim**: Implement SimStorageFile async I/O operations
- **sim**: Add SimStorageProvider wrapping WeakSimWorld
- **sim**: Integrate storage simulation into SimWorld
- **sim**: Add StorageOperation events for simulation
- **sim**: Add InMemoryStorage with deterministic fault injection
- **sim**: Add StorageConfiguration for storage simulation
- **core**: Add StorageProvider and StorageFile traits
- **sim**: Add FDB-aligned chaos features and comprehensive documentation

### 📚 Documentation

- **sim**: Add storage testing patterns and complete doc coverage

### 🚜 Refactor

- **core**: Replace SimulationResult with TimeError in TimeProvider
- **sim**: Simplify storage simulation code
- **sim**: Extract storage operations from world.rs

### 🧪 Testing

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

