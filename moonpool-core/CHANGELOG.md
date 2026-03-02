# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - 2026-03-02

### 🚀 Features

- **moonpool**: Add MoonpoolNode unified actor runtime


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

