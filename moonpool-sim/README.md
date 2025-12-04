# moonpool-sim

Deterministic simulation engine for distributed systems, inspired by [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html).

## Why: Discovery Testing, Not Prevention Testing

Traditional testing asks: *"Did we break what used to work?"* This is **prevention testing**—regression detection through test coverage.

Simulation testing asks: *"What else is broken that we haven't found yet?"* This is **discovery testing**—actively hunting for unknown bugs.

Bugs hide in rare combinations of events. An API with just six variables creates thousands of unique test cases for happy paths alone. Every new feature multiplies the complexity. Traditional testing cannot cover these combinations—but simulation can explore them autonomously.

## How: Deterministic Simulation

**Same seed = identical execution.** Given the same seed, the system makes identical decisions every time. When a bug surfaces after millions of simulated operations, you can replay that exact sequence to debug it.

**Time compression.** A single-threaded event loop advances simulated time when all actors block. Years of uptime can be simulated in seconds. A simulated day passes instantly when nothing is scheduled.

**Chaos injection.** The simulator deliberately biases execution toward rare code paths. Network delays, disconnects, partitions, bit flips—failures that might take months to occur in production happen continuously in simulation.

## Controlled Failure Injection: BUGGIFY

Rather than hoping rare bugs surface, moonpool deliberately triggers them. `buggify!` points fire with 25% probability during testing, creating a combinatorial explosion across configurations.

Strategic placement at error-prone points ensures deep bugs—those needing rare combinations of events—actually get tested.

## The Assertion System

**`always_assert!`**: Invariants that must never fail. These guard correctness properties that should hold regardless of chaos.

**`sometimes_assert!`**: Verify that edge cases actually occur. This is the key insight:

- Prevention testing tells you *"this line was reached"*
- Sometimes assertions verify *"this interesting scenario actually happened"*

If your error handling code exists but `sometimes_assert!` never fires, you haven't actually tested it. The goal is 100% sometimes coverage—proof that every error path was exercised.

## Multi-Seed Testing

Different seeds explore different execution orderings. `UntilAllSometimesReached(N)` runs simulations with different seeds until all `sometimes_assert!` statements have triggered at least once, up to N iterations.

This transforms testing from "check known behaviors" to "explore the unknown until confident."

## Documentation

- [API Documentation](https://docs.rs/moonpool-sim)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
