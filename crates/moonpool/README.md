# moonpool

Deterministic simulation testing for distributed systems in Rust.

Inspired by [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html).

> **Note:** This is a hobby-grade project under active development.

## Architecture

```text
┌─────────────────────────────────────────────────────────────┐
│              moonpool (this crate)                          │
│        Facade over core, simulation, and hyper              │
├──────────────────────────┬──────────────────────────────────┤
│      moonpool-sim        │         moonpool-hyper           │
│  • SimWorld runtime      │  • HTTP/1 and HTTP/2 adapters    │
│  • Chaos testing         │  • Reconnecting h2 channel       │
│  • Assertion macros      │  • tonic and axum integration    │
│  • Exploration           │                                  │
├──────────────────────────┴──────────────────────────────────┤
│                     moonpool-core                           │
│  Provider traits: Time, Task, Network, Random, Storage      │
│  Production implementations through TokioProviders         │
└─────────────────────────────────────────────────────────────┘
```

## Documentation

- [API Documentation](https://docs.rs/moonpool)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
