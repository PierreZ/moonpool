# Moonpool

Deterministic simulation testing for distributed systems in Rust.

Inspired by [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html).

> **Note:** This is a hobby-grade project under active development.

## Quick Start

```bash
# Enter development environment (Nix required)
nix develop

# Run tests
nix develop --command cargo nextest run

# Build documentation
nix develop --command cargo doc --open
```

## Documentation

All documentation lives in Rust doc comments:

```bash
cargo doc --open
```

## License

Apache 2.0
