# Buggify: Fault Injection

<!-- toc -->

- FDB-inspired `buggify!()` macro: scatter fault injection points throughout production code
- Two-phase activation: each location randomly activated once per run, then fires at 25% probability
- `buggify_with_prob!(0.5)` for custom probability
- Five injection patterns: error injection, delay injection, parameter randomization, alternative code paths, process restart
- Probability calibration: high (5-10%) for common edge cases, medium (1%) for standard, low (0.1%) for rare critical
- Anti-patterns: don't use for business logic branching, don't use non-deterministic random, always document the failure scenario
- Buggify never affects production: gated behind simulation context
