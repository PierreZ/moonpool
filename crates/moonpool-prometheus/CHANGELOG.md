# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0] - 2026-09-25

### 🚀 Features

- **moonpool-sim**: SimulationBuilder::run returns Result
- **core**: Typed metric query and reporting layer ([#191](https://github.com/PierreZ/moonpool/pull/191))
- **sim**: Report application metrics from a custom registry ([#190](https://github.com/PierreZ/moonpool/pull/190))

### 🚜 Refactor

- Simplify and factorize every crate ([#274](https://github.com/PierreZ/moonpool/pull/274))
- Delete dead surface, stop the RNG rewind on restart, make numeric casts safe ([#200](https://github.com/PierreZ/moonpool/pull/200))

### 📦 Other

- Close packaging metadata gaps across the publishable crates

