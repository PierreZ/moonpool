# Assertions: Finding Bugs

<!-- toc -->

Imagine your simulation catches a process violating an invariant. You have two choices: crash immediately, or record the violation and keep going. In traditional testing, we crash. The logic seems obvious: something went wrong, stop everything, report the failure.

But that logic is wrong for simulation testing.

## Why Assertions Never Crash

Here is the core insight: **the first thing that goes wrong is rarely the worst thing that goes wrong**. When a process violates a consistency invariant, that violation is a bug. But if we abort the simulation right there, we will never see what happens next. Maybe that corrupted state propagates to two other processes. Maybe the replication protocol silently accepts the bad data and commits it to all replicas. Maybe the system continues operating "normally" for a thousand more steps before the corruption surfaces in a way that would actually harm a user.

An early abort masks the cascade. And the cascade is where the real damage lives.

Moonpool follows the principle that Antithesis pioneered: **assertions record violations and continue**. When `assert_always!` detects a violation, it logs an error, increments a counter, and lets the simulation keep running. The simulation report at the end shows everything that went wrong, in what order, and how often.

This is not about being lenient. It is about being thorough. A single simulation run that records three violations across two different subsystems tells you far more than three separate runs that each crash on the first violation they find.

## Dual Purpose

Assertions in moonpool serve two purposes that reinforce each other.

**First, they verify correctness.** An `assert_always!` is a property that must hold every time it is checked. If it fails even once across thousands of iterations, there is a bug. The system tracks pass and fail counts, giving you a precise success rate rather than a binary pass/fail.

**Second, they guide exploration.** When you enable multiverse mode (covered in Part V), certain assertions become active signals to the explorer. An `assert_sometimes!` that fires true for the first time tells the explorer "this is interesting, branch from here." The explorer snapshots that moment and spawns new timelines from it. This is what turns a random walk through state space into a directed search.

The same assertion does both jobs. You write it once, thinking about correctness. The exploration framework uses it automatically to find more bugs.

## The Reporting Pipeline

After each simulation iteration, the runner checks `has_always_violations()`. If any always-type assertion failed during the iteration, the runner marks that seed as a failure. But the simulation is not interrupted mid-run. The entire iteration completes, accumulating all violations.

When the simulation finishes, `get_assertion_results()` returns an `AssertionStats` for every tracked assertion:

```rust
pub struct AssertionStats {
    /// Total number of times this assertion was evaluated
    pub total_checks: usize,
    /// Number of times the assertion condition was true
    pub successes: usize,
}
```

You can ask for `success_rate()` to get a percentage. An always-assertion with a 99.7% success rate means it failed in 0.3% of checks. That is a bug, and the numbers tell you how hard it is to trigger.

At the end of all iterations, `validate_assertion_contracts()` performs final validation. It returns two categories of violations: **always violations** (definite bugs where invariants were broken) and **coverage violations** (sometimes-assertions that were never satisfied, meaning the simulation did not exercise certain paths). The distinction matters because always violations indicate bugs regardless of iteration count, while coverage violations are only meaningful after enough iterations for statistical confidence.

## What Comes Next

The next four chapters walk through the assertion types from simple to complex. We start with the conceptual taxonomy (invariants, discovery, guidance), then cover boolean assertions, numeric watermarks, and compound assertions. Each type has a different relationship with the simulation and the explorer, and understanding that relationship is the key to writing assertions that actually find bugs.
