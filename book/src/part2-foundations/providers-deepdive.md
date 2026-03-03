# Deep Dive: Why Providers Exist

<!-- toc -->

- Connection to "From Mocks to Simulation": providers ARE the trait-based DI pattern
- Why not `#[cfg(test)]`: production code must be the tested code
- Why not mocks: providers are full implementations with real behavior, not recorded expectations
- The Oxide insight: define the trait, implement twice, inject via generics
- Compile-time guarantee: if it compiles with providers, it works in both contexts
