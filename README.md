# Moonpool

<p align="center">
  <img src="images/logo.png" alt="Moonpool Logo" />
</p>

A Rust toolbox for building distributed systems with deterministic simulation testing, inspired by FoundationDB's simulation testing approach.

> **Note:** This is currently a hobby-grade project under active development.

## Overview

Moonpool provides a comprehensive framework for developing and testing distributed systems through deterministic simulation. Write your distributed system once and test it with simulated networking for predictable debugging, then deploy with real networking - all using the same code.

## Features

- **Deterministic Simulation** - Reproducible testing with controlled time and event ordering
- **Network Abstraction** - Seamlessly swap between simulated and real networking
- **Fault Injection** - Test resilience with configurable delays and packet loss
- **Statistical Testing** - Run multiple iterations with comprehensive reporting
- **Single-Core Design** - Simplified async without thread-safety complexity

## Current Status

The core simulation framework (Phases 1-3) is complete and functional:
- ✅ **Phase 1:** Event queue, time engine, and simulation harness
- ✅ **Phase 2:** Network abstraction with simulated and real implementations
- ✅ **Phase 3:** Statistical testing and comprehensive reporting

## Getting Started

```bash
# Enter development environment
nix develop

# Run tests
cargo nextest run

# Build the project
cargo build
```

## Project Structure

- `moonpool-simulation/` - Core simulation framework
- `docs/specs/` - Technical specifications
- `docs/plans/` - Implementation roadmaps

## License

Apache 2.0