# Numeric Assertions

<!-- toc -->

- `assert_always_gt!(left, right, "message")`: left must always be greater than right
- `assert_always_ge!`, `assert_always_lt!`, `assert_always_le!`: variants
- `assert_sometimes_gt!(left, right, "message")`: the system should eventually achieve left > right
- `assert_sometimes_ge!`, `assert_sometimes_lt!`, `assert_sometimes_le!`: variants
- Watermark tracking: the system remembers the best value observed
- Boundary-seeking behavior: `ALWAYS_LESS_THAN(x, 100)` implicitly drives exploration to maximize x
- Use cases: latency bounds, throughput targets, resource limits
