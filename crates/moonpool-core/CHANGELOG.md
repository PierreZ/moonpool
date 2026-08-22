# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0] - 2026-07-09

### 🚀 Features

- **moonpool-core**: Deterministic select! rotation combinator
- **moonpool-core**: Granular tokio provider features and prelude

### 🐛 Bug Fixes

- Address review findings on the executor branch
- **moonpool-core**: Gate named-task Builder behind cfg(tokio_unstable)

### 🚜 Refactor

- **moonpool-core**: Remove task naming from TokioTaskProvider
- **moonpool-core**: Select! enters tokio's expansion with a seeded offset
- Migrate select!/pin!/watch off tokio entropy
- **core**: Generate OpenOptions flag getters with a macro

### 📦 Other

- Drop the tokio_unstable requirement


## [0.7.0] - 2026-06-05

### 🚀 Features

- **core**: Make moonpool-core wasm-buildable and abstract TaskProvider JoinHandle

### 🐛 Bug Fixes

- **rebase**: Reconcile with main's futures::io and CoveragePlateau

### 🚜 Refactor

- **core**: Derive Default for unit-struct Tokio providers
- **core**: Macro-fy OpenOptions flag setters
- **core,sim**: Collapse provider getter forwards via macro
- **core,sim,transport,derive**: Post-migration cleanup of Send-bounded traits
- **core,sim**: Switch trait impls back to async fn syntax
- **core,transport,derive**: Migrate transport to Send-bounded traits
- **core**: Migrate provider traits to native AFIT
- **core**: Drop unused NetworkAddress flag helpers
- **core**: Drop redundant task_name_clone in spawn_task
- **core**: Collapse timeout match to map_err
- **core**: Drop unused OpenOptions::read_write helper
- **core**: Drop unused Endpoint::is_valid
- **core**: Drop unused NodeAddress type
- **core**: Switch StorageFile IO bounds to futures::io
- **core**: Switch NetworkProvider IO bounds to futures::io

### 🎨 Styling

- **core**: Remove stray blank line after flag_setters! macro
- **core**: Group Storage associated type with the others in TokioProviders

### ⚙️ Miscellaneous Tasks

- **workspace**: Enable clippy pedantic and fix all warnings


## [0.6.0] - 2026-03-28

### 🚜 Refactor

- **moonpool-sim**: Rework workload lifecycle with event-loop integration ([#52](https://github.com/PierreZ/moonpool/pull/52))


## [0.5.0] - 2026-03-09

### 🚀 Features

- **transport**: Add FailureMonitor for address/endpoint failure tracking
- **moonpool**: Add MoonpoolNode unified actor runtime

### 🐛 Bug Fixes

- **moonpool**: Replace HashMap/HashSet with BTreeMap/BTreeSet for deterministic simulation


## [0.4.0] - 2026-02-19

### 🚀 Features

- **core**: Add StorageProvider and StorageFile traits

### 🐛 Bug Fixes

- **moonpool**: Fix metastable replay determinism with seeded tokio runtime

### 📚 Documentation

- Update all READMEs, CLAUDE.md, and rustdoc for 6-crate architecture

### 🚜 Refactor

- **core**: Replace SimulationResult with TimeError in TimeProvider


## [0.3.0] - 2026-01-25

### 🚜 Refactor

- **transport**: Consolidate provider type params into Providers bundle
- **transport**: Improve code quality with 3 fixes

### ⚙️ Miscellaneous Tasks

- Remove false FDB-compatible protocol claims


## [0.2.2] - 2025-12-18

### 🐛 Bug Fixes

- **docs**: Use correct docs.rs metadata keys for tokio_unstable


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

