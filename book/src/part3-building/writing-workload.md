# Writing a Workload

<!-- toc -->

- Step-by-step: implement `Workload` trait
- `setup()`: establish connections to processes, prepare test state
- `run()`: drive requests, inject operations, use `assert_sometimes!` for coverage
- `check()`: validate final system state, use `assert_always!` for invariants
- Access topology via `SimContext` to find process addresses
