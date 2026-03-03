# Always and Sometimes

<!-- toc -->

- `assert_always!(condition, "message")`: must hold every time it's evaluated — violation means bug
- `assert_always_or_unreachable!(condition, "message")`: must hold if reached, passes if never encountered
- `assert_sometimes!(condition, "message")`: must fire true at least once across all iterations
- `assert_reachable!("message")`: code path must be reached at least once
- `assert_unreachable!("message")`: code path must never be reached
- When to use each: always for invariants, sometimes for coverage, reachable for path confirmation
- The "Sometimes as exploration amplifier" insight: when true, the explorer snapshots and branches from that point
