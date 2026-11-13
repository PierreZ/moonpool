# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0] - 2026-11-13

### 🚀 Features

- **site**: Technique pages, playground, extras and the finished site ([#425](https://github.com/PierreZ/moonpool/pull/425))
- **moonpool-assertions**: Raise the slot table to 8192 sites ([#407](https://github.com/PierreZ/moonpool/pull/407))
- **moonpool-assertions**: Raise the slot table to 4096 sites ([#406](https://github.com/PierreZ/moonpool/pull/406))
- **moonpool-sim**: Declare assertion sites at link time, index the slot table ([#393](https://github.com/PierreZ/moonpool/pull/393))
- **moonpool-buggify**: Hint!, the code names a moment and the simulator strikes
- **moonpool-assertions**: 64-bit message hash and 2048 assertion slots ([#273](https://github.com/PierreZ/moonpool/pull/273))
- **explorer**: Strengthen semantic guidance ([#168](https://github.com/PierreZ/moonpool/pull/168))

### 🐛 Bug Fixes

- **moonpool-sim**: Runner-loop fixes (#452, #454, #458, #459, #460, #464) ([#519](https://github.com/PierreZ/moonpool/pull/519))
- **moonpool-assertions**: Wait for a slow initializer and clamp quality values ([#508](https://github.com/PierreZ/moonpool/pull/508))
- **moonpool-assertions**: Truncate stored messages on a char boundary
- **moonpool-assertions**: Never silently merge or drop assertion accounting
- **moonpool-assertions**: Unsigned quality ordering and loud bucket overflow
- **moonpool-assertions**: Report slot table overflow ([#180](https://github.com/PierreZ/moonpool/pull/180))

### 📚 Documentation

- Rewrite the README, the book and the crate docs for every feature
- Rebuild the Claude Code skills and agents for moonpool development ([#207](https://github.com/PierreZ/moonpool/pull/207))

### 🚜 Refactor

- **moonpool-assertions**: Share the dropped-allocation counter helpers
- Simplify and factorize every crate ([#274](https://github.com/PierreZ/moonpool/pull/274))
- Simplify retained crate internals
- Move rust crates under crates

### ⚙️ Miscellaneous Tasks

- Bump Rust to 1.99 and update the Nix flake

### 📦 Other

- Close packaging metadata gaps across the publishable crates


## [0.8.0] - 2026-07-09

### 🚀 Features

- **moonpool-sim**: Consolidate stop conditions into UntilCoverageStable ([#135](https://github.com/PierreZ/moonpool/pull/135))

### 🚜 Refactor

- **moonpool-explorer**: Extract assertion accounting into moonpool-assertions

