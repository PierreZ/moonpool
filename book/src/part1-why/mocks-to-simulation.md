# From Mocks to Simulation

<!-- toc -->

- Why mocks fail: they jump abstraction layers, require deep knowledge of internal call stacks, break on refactor
- The `#[cfg(test)]` trap: conditional compilation means you're not testing real code paths
- The alternative: trait-based simulation — define a trait, implement once for production, once for simulation
- The fidelity spectrum: no-op → in-memory → full simulation → controlled simulation
- Choose minimum fidelity needed per test context
- Error injection over mock expectations: validate how the system recovers, not just whether it calls the right methods
- This is exactly what moonpool's provider pattern implements
