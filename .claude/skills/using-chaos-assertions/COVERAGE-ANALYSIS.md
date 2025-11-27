# Coverage Analysis Guide

How to interpret assertion coverage reports and fix coverage gaps.

## Reading Coverage Reports

After simulation completes, the framework reports sometimes_assert! success rates:

```
Assertion Coverage Report:
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
✓ connection_established: 892/1000 (89.2%)
✓ request_timeout: 234/1000 (23.4%)
✓ retry_attempted: 456/1000 (45.6%)
✗ partition_detected: 0/1000 (0.0%) ← UNTESTED
✗ pool_exhausted: 0/1000 (0.0%) ← UNTESTED
✓ high_load: 123/1000 (12.3%)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Status: FAILED (2 assertions never succeeded)
```

### Interpreting Results

**✓ Good Coverage (>0%)**:
- Path tested at least once
- Percentage shows how frequently condition occurs
- Low percentage (1-5%) is OK if path is rare

**✗ Missing Coverage (0%)**:
- Path never executed across all seeds
- **Blocker**: Must fix before considering test complete

## Common Coverage Issues

### Issue 1: Assertion Never Succeeds (0%)

**Example**:
```
✗ partition_detected: 0/1000 (0.0%)
```

**Diagnosis Steps**:

1. **Is the code path reachable?**
   ```rust
   if self.is_partitioned(target) {
       tracing::warn!("Partition check: {}", self.is_partitioned(target));
       sometimes_assert!(partition_detected, true, "Network partition detected");
       return Err(NetworkError::Partitioned);
   }
   ```

   Check logs: Does "Partition check" appear? If no, path unreachable.

2. **Is the condition ever true?**
   ```rust
   tracing::info!("Partition state: {:?}", self.partition_state);
   ```

   If partition_state is always empty, condition never true.

3. **Is workload generating relevant operations?**
   - Review operation alphabet
   - Check if partition operations are included
   - Verify topology supports partitions

**Solutions**:

**A. Add Missing Operations**:
```rust
enum Operation {
    // Existing
    SendMessage(NodeId, Message),

    // Add partition operations
    CreatePartition(NodeId, NodeId),
    HealPartition(NodeId, NodeId),
}
```

**B. Increase Buggify Probability**:
```rust
// Before: Default 25%
if buggify!() {
    self.create_partition();
}

// After: Higher probability
if buggify_with_prob!(0.75) {
    self.create_partition();
}
```

**C. Increase Iterations**:
```rust
// Before: 1000 iterations
.set_iteration_control(IterationControl::UntilAllSometimesReached(1000))

// After: More iterations
.set_iteration_control(IterationControl::UntilAllSometimesReached(10_000))
```

### Issue 2: Very Low Coverage (<1%)

**Example**:
```
✓ pool_exhausted: 3/1000 (0.3%)
```

Path tested but very rare. Consider if this is acceptable:

**Acceptable if**:
- Edge case that's genuinely rare
- Still reached >0 times (coverage confirmed)
- Test suite completes in reasonable time

**Not acceptable if**:
- Core functionality
- Critical error path
- Should occur more frequently

**Solutions**:

**A. Force Condition More Aggressively**:
```rust
// Before: Normal pool size
let max_size = if buggify!() { 5 } else { 100 };

// After: Smaller pool, more pressure
let max_size = if buggify!() { 2 } else { 100 };
```

**B. Generate More Relevant Load**:
```rust
// Before: Random operations
for _ in 0..100 {
    execute(random_op());
}

// After: Targeted operations to stress pool
for _ in 0..100 {
    execute(Operation::AcquireFromPool);  // Force pool pressure
}
```

### Issue 3: Unexpectedly High Coverage (>90%)

**Example**:
```
✓ connection_failure: 956/1000 (95.6%)
```

Success path rarely works - indicates problem:

**Possible causes**:
- Buggify too aggressive
- System too fragile
- Test setup issue

**Solutions**:

**A. Reduce Buggify Probability**:
```rust
// Before: Too aggressive
if buggify!() {  // 25%
    return Err(ConnectionError::Failed);
}

// After: More conservative
if buggify_with_prob!(0.1) {  // 10%
    return Err(ConnectionError::Failed);
}
```

**B. Check System Configuration**:
```rust
// Before: Impossible configuration
let timeout = Duration::from_millis(1);  // Always times out

// After: Reasonable timeout
let timeout = if buggify!() {
    Duration::from_millis(10)
} else {
    Duration::from_secs(5)
};
```

## Systematic Coverage Analysis

### Step 1: Categorize Assertions

Group by type:

```
Error Handling:
  ✓ connection_failure: 234/1000 (23.4%)
  ✓ timeout_occurred: 189/1000 (18.9%)
  ✗ partition_detected: 0/1000 (0.0%) ← FIX

Load Conditions:
  ✓ high_load: 123/1000 (12.3%)
  ✗ pool_exhausted: 0/1000 (0.0%) ← FIX

State Coverage:
  ✓ multiple_connections: 678/1000 (67.8%)
  ✓ actor_activated: 892/1000 (89.2%)
```

### Step 2: Prioritize Fixes

**High Priority (Must Fix)**:
- Core functionality paths at 0%
- Critical error handling at 0%
- Safety-critical scenarios at 0%

**Medium Priority**:
- Edge cases <1%
- Load conditions <5%
- Rare errors <10%

**Low Priority (Monitor)**:
- Already >10% coverage
- Genuinely rare scenarios
- Nice-to-have coverage

### Step 3: Fix Systematically

For each 0% assertion:

1. **Verify reachability** (add logging)
2. **Check condition** (can it ever be true?)
3. **Review workload** (generates necessary operations?)
4. **Adjust chaos** (buggify probability, resource limits)
5. **Re-run** and verify coverage improves

### Step 4: Iterate

```bash
# Run test
cargo nextest run slow_simulation_my_test

# Check report
# Fix identified gaps
# Re-run

# Repeat until all assertions >0%
```

## Coverage Patterns

### Pattern: Complementary Coverage

Ensure both paths tested:

```
✓ operation_succeeded: 756/1000 (75.6%)
✓ operation_failed: 244/1000 (24.4%)
```

If one is 100% and other 0%, only one path works.

### Pattern: Balanced Error Coverage

```
✓ error_timeout: 89/1000 (8.9%)
✓ error_not_found: 67/1000 (6.7%)
✓ error_connection_lost: 45/1000 (4.5%)
```

All error paths tested, even if rarely.

### Pattern: Load Spectrum

```
✓ low_load: 678/1000 (67.8%)
✓ moderate_load: 234/1000 (23.4%)
✓ high_load: 88/1000 (8.8%)
```

Coverage across load spectrum from normal to extreme.

## Debugging Coverage Gaps

### Technique 1: Targeted Seed Search

Find seed that triggers path:

```rust
for seed in 0..10000 {
    let report = SimulationBuilder::new()
        .set_seed(seed)
        .set_iteration_control(IterationControl::FixedCount(1))
        .run()
        .await;

    if report.assertion_succeeded("partition_detected") {
        println!("Path triggered with seed: {}", seed);
        break;
    }
}
```

### Technique 2: Forced Execution

Temporarily force condition:

```rust
// Temporarily: Always create partition
if true {  // Was: buggify!()
    sometimes_assert!(partition_detected, true, "Partition detected");
    self.create_partition();
}

// Verify assertion succeeds
// Then restore: if buggify_with_prob!(0.75) { ... }
```

### Technique 3: Workload Simplification

Reduce operations to focus on gap:

```rust
// Before: 1000 random operations
for _ in 0..1000 {
    execute(random_op());
}

// After: Only partition-related operations
for _ in 0..100 {
    execute(Operation::CreatePartition(n1, n2));
    execute(Operation::SendAcrossPartition(n1, n2));
    execute(Operation::HealPartition(n1, n2));
}

// Check if partition_detected assertion now succeeds
```

## Final Checklist

Before considering coverage complete:

- [ ] All sometimes_assert! statements have >0% success rate
- [ ] Both success and failure paths tested (where applicable)
- [ ] Error handling paths all tested
- [ ] Load spectrum covered (low, medium, high)
- [ ] State transitions tested
- [ ] Multi-actor scenarios tested
- [ ] No unexpected 100% or 0% outliers
- [ ] Coverage gaps have documented reasons or fixes

## Summary

**Coverage analysis is iterative**:
1. Run simulation
2. Identify gaps (0% assertions)
3. Diagnose root cause
4. Fix (add operations, tune buggify, increase iterations)
5. Re-run
6. Repeat until complete

**Goal**: Prove every important code path executes at least once under chaos.
