# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0] - 2026-11-20

### 🚀 Features

- **moonpool-sim**: SimulationBuilder::run returns Result
- **core**: Typed metric query and reporting layer ([#191](https://github.com/PierreZ/moonpool/pull/191))
- **sim**: Report application metrics from a custom registry ([#190](https://github.com/PierreZ/moonpool/pull/190))

### 🐛 Bug Fixes

- **moonpool-prometheus**: Refuse shape-mismatched re-registration; docs drifts (#476, #478) ([#523](https://github.com/PierreZ/moonpool/pull/523))
- **moonpool-prometheus**: Key labelled series the way the scrape does

### 📚 Documentation

- Rewrite the README, the book and the crate docs for every feature

### 🚜 Refactor

- Remove RPC load balancing, JWT, the IP allow list and the metric query layer ([#540](https://github.com/PierreZ/moonpool/pull/540))
- Remove the Orleans analysis, unused public items and six numeric assertion macros
- Simplify and factorize every crate ([#274](https://github.com/PierreZ/moonpool/pull/274))
- Delete dead surface, stop the RNG rewind on restart, make numeric casts safe ([#200](https://github.com/PierreZ/moonpool/pull/200))

### 📦 Other

- Bump flake and Rust 1.100.0; pass clippy pedantic with no allows ([#557](https://github.com/PierreZ/moonpool/pull/557))
- Test-only moonpool dependencies carry no version
- Close packaging metadata gaps across the publishable crates

