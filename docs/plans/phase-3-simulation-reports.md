# Phase 3: Simulation Reports and Testing Framework

## Overview

Phase 3 implements the simulation report system, workload registration, and assertion framework to enable comprehensive distributed systems testing. This phase builds on the networking foundation from Phase 2 to add statistical analysis and chaos testing capabilities.

## Phase 3a: Thread-Local RNG Migration

**Goal**: Replace the current RNG in SimWorld with thread-local RNG for cleaner API design.

### Current State Analysis

The current implementation has RNG embedded in the `SimWorld` struct, requiring it to be passed through the simulation state. This creates API complexity and potential borrowing issues.

### Implementation Steps

1. **Add thread-local RNG module**
   - Create `src/rng.rs` module
   - Implement `sim_random()` and `set_sim_seed()` functions
   - Use `ChaCha8Rng` for deterministic behavior
   - Add thread-local storage with `RefCell` wrapper

2. **Handle parallel test execution**
   - **Critical**: Each test gets its own thread, so thread-local state is isolated
   - Design RNG reset/initialization for each simulation run
   - Ensure clean state between consecutive simulations on same thread
   - Add `reset_sim_rng()` function to clear thread-local state

3. **Update SimWorld to use thread-local RNG**
   - Remove RNG field from `SimWorld` struct
   - Update `SimWorld::new()` to set thread-local seed instead
   - **Important**: Call `reset_sim_rng()` before setting new seed
   - Replace direct RNG usage with `sim_random()` calls

4. **Update network simulation to use thread-local RNG**
   - Replace RNG parameters in network fault injection
   - Use `sim_random()` for packet loss, delays, etc.
   - Ensure deterministic behavior is preserved

5. **Add comprehensive tests for thread-local RNG**
   - Test seed determinism across multiple calls
   - Verify isolation between different seeds
   - Test RNG state persistence within single thread
   - **Critical**: Test parallel execution with different seeds
   - Test consecutive simulations on same thread with different seeds
   - Verify no cross-contamination between parallel tests

### API Design

```rust
// New thread-local RNG API
pub fn sim_random<T>() -> T 
where 
    StandardDistribution: Distribution<T>;

pub fn sim_random_range<T>(range: std::ops::Range<T>) -> T
where
    T: SampleUniform;

pub fn set_sim_seed(seed: u64);

// Critical for parallel test execution
pub fn reset_sim_rng();

// SimWorld initialization pattern for clean state
impl SimWorld {
    pub fn new_with_seed(seed: u64) -> Self {
        reset_sim_rng();  // Clear any previous state
        set_sim_seed(seed);
        // ... rest of initialization
    }
}

// Usage in simulation code
if sim_random::<f64>() < packet_loss_rate {
    // Drop packet
}

let delay = sim_random_range(min_delay..max_delay);
```

### Success Criteria

- ✅ All existing tests pass with thread-local RNG
- ✅ SimWorld no longer contains RNG field
- ✅ Same seed produces identical behavior
- ✅ Clean API without RNG parameters
- ✅ No borrowing conflicts from RNG usage

## Phase 3b: Basic Assertion Macros

**Goal**: Implement `always_assert!` and `sometimes_assert!` macros with basic reporting.

### Implementation Steps

1. **Create assertion module**
   - Add `src/assertions.rs` module
   - Implement assertion result tracking
   - Add thread-local assertion storage

2. **Implement assertion macros**
   - `always_assert!` macro with panic on failure
   - `sometimes_assert!` macro with result tracking
   - Basic assertion result collection

3. **Add assertion integration to SimWorld**
   - Collect assertion results during simulation
   - Provide access to assertion statistics
   - Reset assertion state between runs

4. **Add tests for assertion macros**
   - Test assertion tracking across multiple calls
   - Verify statistical collection for sometimes assertions
   - Test assertion state isolation

### API Design

```rust
// Assertion macros
always_assert!(leader_exists, { leader.is_some() }, "Leader must exist");
sometimes_assert!(fast_consensus, { time < threshold }, "Quick consensus");

// Access assertion results
let results = sim.assertion_results();
println!("Fast consensus rate: {:.2}%", results["fast_consensus"].success_rate);
```

## Phase 3c: Simple Simulation Report

**Goal**: Implement basic SimulationReport with core metrics collection.

### Implementation Steps

1. **Create report structures**
   - `SimulationReport` struct with core metrics
   - Track total time, simulated time, iterations
   - Include assertion results summary

2. **Implement basic SimulationBuilder**
   - Simple builder pattern for running simulations
   - Single workload registration initially
   - Random seed generation

3. **Add report generation**
   - Collect metrics during simulation runs
   - Aggregate results across multiple iterations
   - Generate comprehensive report

4. **✅ COMPLETED: Re-enabled SimulationBuilder for combined workloads**
   - ✅ Added `sleep()` method to SimNetworkProvider for workload coordination
   - ✅ Fixed SimulationBuilder to process events concurrently with workloads using `tokio::select!`
   - ✅ Re-enabled combined server+client workload approach in ping-pong test
   - ✅ Ensured proper event processing and simulation time advancement in SimulationBuilder
   - ✅ All tests now pass including concurrent workload scenarios

### API Design

```rust
let report = SimulationBuilder::new()
    .set_iterations(100)
    .register_workload("ping_pong", |seed| async move {
        set_sim_seed(seed);
        run_ping_pong_test().await
    })
    .run().await;

println!("Success rate: {:.2}%", report.success_rate());
```


## Implementation Order

1. **Phase 3a**: Thread-local RNG (1-2 days)
   - Foundational change that simplifies all future work
   - Clean up API before adding complexity

2. **Phase 3b**: Assertion macros (1 day)
   - Basic building blocks for testing
   - Simple implementation with thread-local storage

3. **Phase 3c**: Simulation report (2-3 days)
   - Core infrastructure for multi-iteration testing
   - Foundation for statistical analysis

## Total Estimated Time: 4-6 days

## Dependencies

- Phase 2 networking implementation must be complete
- All Phase 2 tests must pass before starting Phase 3
- Maintain backward compatibility with existing Phase 2 API

## Success Metrics

- Clean thread-local RNG API reduces code complexity
- Assertion macros provide useful testing capabilities
- Simulation reports enable statistical analysis of distributed systems
- All existing functionality continues to work
- Comprehensive test coverage for all new features