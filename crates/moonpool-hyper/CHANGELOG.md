# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0] - 2026-09-25

### 🚀 Features

- **moonpool-hyper**: Add explicit channel shutdown ([#170](https://github.com/PierreZ/moonpool/pull/170))

### 🐛 Bug Fixes

- **moonpool-hyper**: Mint HyperTimer's anchor without a clock on wasm32-unknown-unknown
- Instrument the mutating and fault-injecting entry points
- **moonpool-hyper**: Bound H2Server's graceful drain with a deadline
- **moonpool-hyper**: Expire reset streams by count, never by the wall clock ([#209](https://github.com/PierreZ/moonpool/pull/209))
- Make multi-channel gRPC replay deterministic ([#173](https://github.com/PierreZ/moonpool/pull/173))

### 🚜 Refactor

- Simplify and factorize every crate ([#274](https://github.com/PierreZ/moonpool/pull/274))
- Delete dead surface, stop the RNG rewind on restart, make numeric casts safe ([#200](https://github.com/PierreZ/moonpool/pull/200))
- Simplify retained crate internals
- Move rust crates under crates

### 📦 Other

- Close packaging metadata gaps across the publishable crates

