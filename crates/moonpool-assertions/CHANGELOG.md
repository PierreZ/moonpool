# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0] - 2026-09-25

### 🚀 Features

- **moonpool-assertions**: 64-bit message hash and 2048 assertion slots ([#273](https://github.com/PierreZ/moonpool/pull/273))
- **explorer**: Strengthen semantic guidance ([#168](https://github.com/PierreZ/moonpool/pull/168))

### 🐛 Bug Fixes

- **moonpool-assertions**: Never silently merge or drop assertion accounting
- **moonpool-assertions**: Unsigned quality ordering and loud bucket overflow
- **moonpool-assertions**: Report slot table overflow ([#180](https://github.com/PierreZ/moonpool/pull/180))

### 📚 Documentation

- Rebuild the Claude Code skills and agents for moonpool development ([#207](https://github.com/PierreZ/moonpool/pull/207))

### 🚜 Refactor

- Simplify and factorize every crate ([#274](https://github.com/PierreZ/moonpool/pull/274))
- Simplify retained crate internals
- Move rust crates under crates

### 📦 Other

- Close packaging metadata gaps across the publishable crates


## [0.8.0] - 2026-07-09

### 🚀 Features

- **moonpool-sim**: Consolidate stop conditions into UntilCoverageStable ([#135](https://github.com/PierreZ/moonpool/pull/135))

### 🚜 Refactor

- **moonpool-explorer**: Extract assertion accounting into moonpool-assertions

