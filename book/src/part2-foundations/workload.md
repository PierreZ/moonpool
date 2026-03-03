# Workload: Your Test Driver

<!-- toc -->

- The `Workload` trait: `name()`, `setup()`, `run()`, `check()`
- Lifecycle: setup runs sequentially → run executes concurrently → check validates sequentially
- Workloads survive reboots: they observe the system from outside
- Driving requests: use providers to connect to processes, send traffic, observe responses
- Checking correctness: `check()` runs after simulation completes — validate final state
- Multiple workloads: `WorkloadCount::Fixed(n)` or `Random(range)` for concurrent drivers
