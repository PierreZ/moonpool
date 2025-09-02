# Phase 5: Module-Based Unreachable Code Detection System

## Overview

Implement a comprehensive unreachable code detection system for `sometimes_assert!` macros that can identify assertions registered at compile-time but never executed during runtime, indicating potentially unreachable code paths within active modules.

## Problem Statement

The current dynamic validation system only detects `sometimes_assert!` calls that are executed but have problematic success rates (0% or 100%). However, it cannot detect assertions that are never reached due to unreachable code paths. This is a critical gap identified by Antithesis best practices for distributed systems testing.

## Requirements

### Functional Requirements

1. **Compile-Time Registration**: All `sometimes_assert!` calls must be registered globally when code is compiled, regardless of execution
2. **Module-Based Scoping**: Use Rust's module system for automatic scope detection to avoid false positives from unexecuted modules
3. **Runtime Tracking**: Track which modules have executed at least one assertion during test runs
4. **Unreachable Detection**: Identify assertions registered in active modules but never executed
5. **Enhanced Validation**: Report both existing success rate violations and new unreachable code violations
6. **Backward Compatibility**: All existing functionality must remain unchanged

### Non-Functional Requirements

1. **Zero Configuration**: System must work automatically without manual scope declarations
2. **Thread Safety**: Support concurrent test execution without conflicts
3. **Performance**: Minimal overhead on assertion execution
4. **Clear Reporting**: Distinguish between different types of violations in output

## Architecture Design

### Data Structures

```rust
// Global compile-time registry
static REGISTERED_ASSERTIONS: LazyLock<Mutex<HashMap<String, HashSet<String>>>> = ...;
// Key: module_path, Value: set of assertion names

// Thread-local runtime tracking  
thread_local! {
    static ACTIVE_MODULES: RefCell<HashSet<String>> = ...;
    static ASSERTION_RESULTS: RefCell<HashMap<String, AssertionStats>> = ...; // existing
}
```

### Modified Macro

```rust
#[macro_export]
macro_rules! sometimes_assert {
    ($name:ident, $condition:expr, $message:expr) => {
        // Compile-time registration
        $crate::assertions::register_assertion_at_compile_time(module!(), stringify!($name));
        
        // Runtime execution
        let result = $condition;
        $crate::assertions::record_assertion_with_module(module!(), stringify!($name), result);
    };
}
```

### Key Functions

```rust
// Compile-time registration (called by macro expansion)
pub fn register_assertion_at_compile_time(module_path: &'static str, name: &'static str);

// Runtime execution tracking (replaces existing record_assertion) 
pub fn record_assertion_with_module(module_path: &str, name: &str, success: bool);

// Enhanced validation (updates existing function)
pub fn validate_assertion_contracts() -> Result<(), ValidationReport>;
```

### Validation Logic

1. **For each active module** (modules that executed ≥1 assertion):
   - Retrieve all registered assertions for that module
   - Retrieve all executed assertions for that module  
   - Report missing assertions as unreachable code

2. **For all executed assertions** (existing logic):
   - Check success rates for 0% or 100% violations

### Reporting Structure

```rust
pub struct ValidationReport {
    pub success_rate_violations: Vec<String>,  // existing: 0%/100% issues
    pub unreachable_assertions: Vec<String>,   // new: never-called assertions
}
```

## Implementation Plan

### Phase 5.1: Core Infrastructure
- Add global registration system using `std::sync::LazyLock`
- Implement compile-time registration function
- Add thread-local active module tracking

### Phase 5.2: Macro Enhancement  
- Modify `sometimes_assert!` macro to register at compile time
- Update runtime recording to track module execution
- Ensure existing functionality remains intact

### Phase 5.3: Validation Logic
- Enhance `validate_assertion_contracts()` function
- Implement unreachable assertion detection
- Add comprehensive error reporting

### Phase 5.4: Reporting Integration
- Update `SimulationReport` to include unreachable assertion results
- Enhance display formatting for new violation types
- Ensure clear distinction between violation types

### Phase 5.5: Testing & Validation
- Add comprehensive tests for unreachable code detection
- Create test cases with deliberately unreachable assertions
- **Modify ping-pong test to demonstrate unreachable code detection**
- Search for and identify real unreachable assertions in existing codebase
- Verify all existing tests continue to pass
- Test thread safety and concurrent execution

### Phase 5.6: Enhanced Iteration Control
- Modify `set_iterations` from runner to accept an enum containing either:
  - A number of seeds to run
  - A time duration to spend running tests
  - Run until all sometimes assertions have been reached (with a safety limit)

## Example Expected Behavior

### Code Example
```rust
// In module: moonpool_simulation::network::peer::tests
#[test]
fn test_connection() {
    sometimes_assert!(fast_connect, latency < 100, "Connection should be fast");  // Executed
    if false {
        sometimes_assert!(retry_works, retries < 3, "Should retry");              // Never called
    }
}
```

### Ping-Pong Test Enhancement
The ping-pong test will be enhanced with strategic `sometimes_assert!` calls in conditional branches to demonstrate the system:

```rust
// Add assertions in error handling paths that may not be exercised
if connection_failed {
    sometimes_assert!(retry_successful, retry_count < 3, "Retries should be limited");
}

// Add assertions in performance monitoring that may not trigger
if latency > threshold {
    sometimes_assert!(fallback_works, fallback_latency < backup_threshold, "Fallback should work");  
}
```

### Expected Validation Output
```
=== Assertion Validation ===
❌ Unreachable code detected:
- retry_successful in module moonpool_simulation::ping_pong_tests was never called
- fallback_works in module moonpool_simulation::ping_pong_tests was never called

✅ Success rate validation passed for executed assertions
```

### Scoping Behavior
- **Run only peer tests**: Validates peer module assertions completely, ignores tcp module
- **Run all tests**: Validates all modules that executed assertions  
- **Module with no assertions executed**: Ignored (no false positives)

## Benefits

1. **True Unreachable Code Detection**: Identifies code paths that are never exercised during testing
2. **Automatic Scope Management**: Uses existing module boundaries without manual configuration
3. **No False Positives**: Only validates modules that were actually executed
4. **Comprehensive Coverage**: Combines existing success rate validation with new unreachability detection
5. **Developer Friendly**: Clear, actionable error messages for improving test coverage

## Success Criteria

- [ ] All `sometimes_assert!` calls are registered at compile time
- [ ] Module execution tracking works correctly across all test scenarios
- [ ] Unreachable assertions are detected and reported accurately
- [ ] No false positives from unexecuted modules
- [ ] All existing tests pass without modification
- [ ] **Ping-pong test successfully demonstrates unreachable code detection**
- [ ] **Real unreachable assertions identified in existing codebase**
- [ ] Clear, distinguishable error messages for different violation types
- [ ] Thread-safe operation in concurrent test environments
- [ ] Performance impact is negligible (< 1% overhead)

## Risks and Mitigations

### Risk: Performance Impact
- **Mitigation**: Use efficient data structures (HashSet/HashMap) and minimize locking
- **Validation**: Benchmark assertion execution before/after implementation

### Risk: Thread Safety Issues
- **Mitigation**: Use proper synchronization primitives (Mutex, RefCell)
- **Validation**: Test concurrent execution scenarios thoroughly

### Risk: False Positives in Complex Scenarios  
- **Mitigation**: Careful module path handling and execution tracking
- **Validation**: Test with various module structures and execution patterns

### Risk: Breaking Existing Functionality
- **Mitigation**: Comprehensive regression testing and phased implementation
- **Validation**: All existing tests must pass at each phase

## Dependencies

- `std::sync::LazyLock` for global registration
- Existing assertion infrastructure in `src/assertions.rs`
- Current `sometimes_assert!` macro implementation
- `SimulationReport` and validation system

## Timeline Estimate

- **Phase 5.1-5.2**: 2-3 hours (Core infrastructure and macro changes)
- **Phase 5.3-5.4**: 2-3 hours (Validation logic and reporting) 
- **Phase 5.5**: 2-4 hours (Testing and validation)
- **Total**: 6-10 hours

## Future Enhancements

- Integration with code coverage tools for comprehensive analysis
- Support for conditional compilation flags (#[cfg]) awareness
- IDE integration for real-time unreachable code highlighting
- Metrics collection on code coverage improvement over time