# moonpool-buggify

Standalone buggify fault-injection macros for the moonpool framework.

## Why

Buggify, from FoundationDB, marks places in your code where a rare but legal
behavior can be forced during simulation: an early timeout, a dropped buffer, a
slow path. Each location is **activated** once per simulation run with a fixed
probability. An active location then **fires** on some of its calls. Bugs that
need a rare branch to be taken become likely instead of lucky.

## Inert outside a simulation

This crate has no dependencies. It holds only the disabled-by-default state and
the `buggify!` / `buggify_with_prob!` macros, so production and sans-I/O code
can depend on it without pulling in a simulation runtime:

- Outside an active simulation every call site evaluates to `false` with no side
  effects.
- A simulation runtime (`moonpool-sim`) installs its seeded random source with
  `set_random_source`, enables buggify with `buggify_init`, and disables it
  again with `buggify_reset`. Every draw comes from that source, so a seed
  replays the same activations and firings.

```rust
use moonpool_buggify::{buggify, buggify_with_prob};

fn flush(buffer: &mut Vec<u8>) {
    if buggify!() {
        // 25% of calls while this location is active: take the slow path.
    }
    if buggify_with_prob!(0.01) {
        // Your own per-site firing rate.
    }
    buffer.clear();
}
```

`buggify_knob!`, which picks an extreme value for a tunable, needs the
simulation's random helpers and is provided by `moonpool-sim`.

## Documentation

- [API Documentation](https://docs.rs/moonpool-buggify)
- [Book chapter](https://pierrez.github.io/moonpool/part3-building/08-buggify.html)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
