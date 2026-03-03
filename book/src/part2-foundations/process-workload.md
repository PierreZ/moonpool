# System Under Test vs Test Driver

<!-- toc -->

- The mental model: two roles in every simulation
- The system under test (Process): the thing you're building, the thing that has bugs
- The test driver (Workload): the thing that exercises the system, validates correctness
- They have different lifecycles: processes can crash and reboot, workloads survive everything
- They have different perspectives: processes see their own state, workloads see the whole system
