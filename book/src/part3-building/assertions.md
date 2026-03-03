# Assertions: Finding Bugs

<!-- toc -->

- The assertion philosophy: assertions never crash the program
- Why: let failures cascade into more severe bugs that would be masked by an early abort
- Assertions record violations and continue — the simulation report shows everything
- Dual purpose: verify correctness AND guide exploration (in multiverse mode)
- `has_always_violations()` checked after each iteration
- `AssertionStats`: `total_checks`, `successes`, `success_rate()`
