# moonpool

Deterministic simulation testing for distributed systems in Rust.

Inspired by [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html).

> **Note:** This is a hobby-grade project under active development.

## Architecture

```text
┌─────────────────────────────────────────────────┐
│           moonpool (this crate)                 │
│         Re-exports all functionality            │
├─────────────────────────────────────────────────┤
│  moonpool-transport    │    moonpool-sim        │
│  • Peer connections    │    • SimWorld runtime  │
│  • Wire format         │    • Chaos testing     │
│  • NetTransport        │    • Buggify macros    │
│  • RPC primitives      │    • Assertions        │
├─────────────────────────────────────────────────┤
│              moonpool-core                      │
│  Provider traits: Time, Task, Network, Random   │
│  Core types: UID, Endpoint, NetworkAddress      │
└─────────────────────────────────────────────────┘
```

## Documentation

- [API Documentation](https://docs.rs/moonpool)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
