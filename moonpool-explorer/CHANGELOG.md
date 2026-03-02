# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - 2026-03-02

### 🚀 Features

- Move simulation tests to per-crate binary targets for sancov compatibility
- **moonpool-explorer**: Add sancov edge coverage to stats and reporting
- **moonpool-explorer**: Wire sancov into fork loop for code-edge coverage signals
- **moonpool-explorer**: Add SANCOV_CRATES build infrastructure for selective sancov instrumentation
- **moonpool-explorer**: Add sancov.rs core module for LLVM inline-8bit-counters

### 📚 Documentation

- **moonpool-explorer**: Add comprehensive rustdoc to sancov.rs


## [0.4.0] - 2026-02-19

### 🚀 Features

- **moonpool-sim**: Enrich simulation report with detailed assertions, buckets, and per-seed metrics
- **moonpool-explorer**: Add coverage-preserving multi-seed exploration
- **moonpool-explorer**: Add multi-core parallel exploration via sliding window fork loop
- **moonpool**: Backport Antithesis assertion suite — 14 macros, rich shared-memory slots
- **moonpool-explorer**: Add EachBucket infrastructure for assert_sometimes_each!
- **moonpool-explorer**: Add adaptive forking, 3-level energy budgets, and exploration report
- **moonpool-explorer**: Add fork-based multiverse exploration crate

### 🐛 Bug Fixes

- **moonpool-explorer**: Set coverage bitmap in assert_sometimes_each, tune exploration configs

### 📚 Documentation

- Align documentation with current codebase state
- **moonpool-explorer**: Comprehensive rustdoc with glossary, ASCII diagrams, and walkthrough
- Update all READMEs, CLAUDE.md, and rustdoc for 6-crate architecture

### 🚜 Refactor

- Unify test naming with slow_simulation_ prefix and nextest profiles

### 🧪 Testing

- **moonpool-explorer**: Add adaptive forking integration tests

