# moonpool-core

Core abstractions for the moonpool simulation framework.

## Why: Interface Swapping

The same application code runs in both simulation and production. The only difference is which provider implementations you use.

This is FoundationDB's core insight: no mocks, no stubs—real code runs in both environments. A global network pointer switches between production (real TCP via tokio) and simulation (fake connections to in-memory buffers).

## How: Provider Traits

Application code depends on traits, not concrete implementations:

| Trait | Purpose | Production | Simulation |
|-------|---------|------------|------------|
| `TimeProvider` | Sleep, timeout, now() | Wall clock | Logical time |
| `NetworkProvider` | Connect, listen, accept | Real TCP | Simulated TCP |
| `TaskProvider` | Spawn async tasks | Tokio spawn | Event-driven |
| `RandomProvider` | Random numbers | System RNG | Seeded RNG |

## The Golden Rule

**Never call tokio directly in application code.**

Instead of `tokio::time::sleep()`, use `time_provider.sleep()`. This ensures your code works identically in simulation and production.

## Core Types

FDB-compatible types for endpoint addressing:

- `UID`: 128-bit unique identifier (deterministically generated in simulation)
- `Endpoint`: Network address + token for direct addressing
- `NetworkAddress`: IP address + port
- `WellKnownToken`: Reserved tokens for system services

## Documentation

- [API Documentation](https://docs.rs/moonpool-core)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
