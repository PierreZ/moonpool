# Compound Assertions

<!-- toc -->

- `assert_sometimes_all!("message", &[("sub-goal-1", bool), ("sub-goal-2", bool), ...])`: all sub-goals must be simultaneously true
  - Frontier tracking: the system explores along the frontier of how many sub-goals are satisfied
  - Use case: multi-step objectives (e.g., "all nodes healthy AND leader elected AND data replicated")
- `assert_sometimes_each!("message", value)`: creates one assertion per distinct value observed
  - Equalizes exploration across all discovered values
  - Prevents over-concentration near starting states
  - Inline Zelda analogy: 128 overworld screens + 230 dungeon rooms, each explored equally
