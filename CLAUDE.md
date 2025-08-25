# Claude Context for Moonpool

## Project Overview
Moonpool is a Rust project designed to create the right toolbox for building distributed systems. The project is structured as a workspace with multiple components.

## Current Focus: Simulation Framework
Currently working on the simulation framework located in `moonpool-simulation`. This is a deterministic simulation framework for testing distributed systems.

### Architecture Constraints
**IMPORTANT**: The simulation framework is designed for single-core execution only. This design decision ensures:
- Deterministic behavior without complex synchronization
- Simplified async implementation without Send/Sync bounds
- Predictable event ordering and timing
- No thread-safety complexity in simulation state

**Networking Traits**: Do NOT add Send bounds to networking traits. Use `#[async_trait(?Send)]` for all async traits in the network module.

### Phase 2 Testing Requirements
**IMPORTANT**: Phase 2 networking implementation must include BOTH types of tests:

1. **Simulation Tests**: Test network behavior using `SimNetworkProvider`
   - Verify simulated connections, message delivery, fault injection
   - Test deterministic behavior and event ordering
   - Focus on simulation-specific features like time control

2. **Tokio Integration Tests**: Test the same code using `TokioNetworkProvider` 
   - Verify real networking works with the same trait implementations
   - Ensure seamless swapping between simulation and real networking
   - Test that application code works identically with both providers

**Test Pattern**: Write tests that run the SAME application logic with both `SimNetworkProvider` and `TokioNetworkProvider` to verify trait compatibility and seamless swapping.

## Development Environment Setup

**IMPORTANT**: Claude must use `nix develop` shell for compilation and testing.

### For Claude Code
When running Rust commands (cargo build, cargo test, etc.), Claude MUST use:
```bash
nix develop --command <cargo-command>
```

Example usage:
- `nix develop --command cargo nextest run` - Run tests with nextest (preferred)
- `nix develop --command cargo test` - Run tests with standard test runner (fallback)
- `nix develop --command cargo build` - Build project
- `nix develop --command cargo check` - Check compilation
- `nix develop --command cargo fmt` - Format code

### Why This Is Required
- The project uses Nix flake for dependency management
- Essential build tools (gcc, pkg-config, etc.) are only available in the Nix shell
- Without `nix develop`, compilation will fail with "linker `cc` not found"
- The flake.nix includes all necessary Rust toolchain and build dependencies

### Alternative: Enter Shell First
If running multiple commands, Claude can enter the shell first:
```bash
nix develop  # Enter the development shell
cargo nextest run   # Then run commands normally
```

## Phase Completion Criteria

**IMPORTANT**: A phase is considered finished only when ALL of the following pass:

```bash
nix develop --command cargo fmt         # Code formatting
nix develop --command cargo clippy      # Linting and code quality
nix develop --command cargo nextest run # All tests pass (preferred)
# OR nix develop --command cargo test   # Fallback if nextest unavailable
```

### Phase Completion Checklist
- ✅ **cargo fmt** - Code is properly formatted
- ✅ **cargo clippy** - No linting warnings or errors  
- ✅ **cargo nextest run** - All tests pass (unit, integration, doc tests)
- ✅ **Functionality** - All phase requirements implemented
- ✅ **Documentation** - Code is documented and examples work

### Documentation Enforcement
The crate uses `#![deny(missing_docs)]` to enforce documentation on all public items. This means:
- **All public functions, structs, enums, and modules MUST have documentation**
- **Clippy will fail compilation if any public item lacks documentation**
- **This ensures consistent, professional API documentation across all phases**

### Error Handling Policy
**IMPORTANT**: `unwrap()` and `expect()` are FORBIDDEN in production code.

**Automatic Enforcement:**
The crate uses `#![deny(clippy::unwrap_used)]` to automatically prevent unwraps in production code.

**Rules:**
- ❌ **NO `unwrap()`** in any production code (src/ directory) - **Clippy will fail compilation**
- ❌ **NO `expect()`** in production code unless absolutely necessary with detailed justification
- ✅ **`unwrap()` ALLOWED** in test code (#[cfg(test)] blocks and tests/ directory)
- ✅ **Use proper error handling** with `Result<T, E>` and `?` operator
- ✅ **Return meaningful errors** using our `SimulationResult<T>` type

**Why This Matters:**
- Prevents unexpected panics in production
- Forces explicit error handling and recovery
- Makes the simulation framework robust and reliable
- Enables graceful degradation rather than crashes

**Acceptable Alternatives:**
- `result.map_err(|e| SimulationError::InvalidState(format!("reason: {}", e)))?`
- `option.ok_or(SimulationError::InvalidState("missing value".to_string()))?`
- Pattern matching with explicit error handling

**Do not consider a phase complete until all cargo commands (fmt, clippy, nextest) pass without warnings or errors.**

## Current Status
All core simulation framework phases (1-3) are complete and ready for production use. The framework provides:
- Deterministic simulation infrastructure 
- Network abstraction with real and simulated implementations
- Comprehensive testing and reporting capabilities

## Project Structure
- `moonpool-simulation/` - Main simulation framework implementation (Phases 1-3 complete)
- `docs/specs/simulation/` - Simulation framework specification
- `docs/plans/` - Implementation plans and roadmaps
- `docs/references/` - Reference code from other projects for inspiration

## Implementation Status: ✅ PHASES 1-3 COMPLETED

### Phase Completion Verification
- ✅ **cargo fmt** - Code properly formatted (no changes needed)
- ✅ **cargo clippy** - No linting warnings or errors
- ✅ **cargo nextest run** - All 85 tests passing

## Phase 1: ✅ COMPLETED - Core Infrastructure
- Event queue with deterministic ordering
- Logical time advancement engine
- Basic simulation harness with handle pattern
- Comprehensive test coverage including deterministic behavior verification
- thiserror integration for better error handling
- Clean module structure with documented public API

## Phase 2: ✅ COMPLETED - Network Simulation  
- Point-to-point connections with bidirectional communication
- Message delivery simulation with configurable latency
- Basic fault injection (delays, packet loss)
- NetworkProvider trait abstraction
- TokioNetworkProvider for real networking
- SimNetworkProvider for simulation
- Comprehensive network configuration system
- Sleep functionality for simulation coordination

## Phase 3: ✅ COMPLETED - Simulation Reports and Testing
- Thread-local RNG migration with deterministic seeding
- Basic assertion macros (`always_assert!` and `sometimes_assert!`)
- SimulationReport system with comprehensive metrics
- SimulationBuilder for multi-iteration testing
- Workload registration and parallel execution
- Statistical analysis and failure tracking
- Clean integration with networking simulation

**All phases meet completion criteria and the simulation framework is ready for production use.**

## Sometimes Assertions Best Practices (Antithesis)

### When to Use Sometimes vs Always Assertions

**Always Assertions (`always_assert!`):**
- Guard against unexpected program states that should NEVER occur
- Verify invariants that must hold in every execution
- Check critical safety properties
- Example: "Server should always receive valid protocol messages"

**Sometimes Assertions (`sometimes_assert!`):**
- Verify that important code paths are actually being tested
- Check rare but important scenarios are reachable
- Validate error handling and recovery paths are exercised
- Example: "Connection retry logic should sometimes be triggered"

### What Makes a Good Sometimes Assertion

1. **Target Rare Events**: Focus on low-probability but high-impact scenarios
2. **Error Path Coverage**: Place in error handling and recovery code
3. **Boundary Conditions**: Check edge cases and limits are tested
4. **Race Conditions**: Verify concurrent scenarios are explored
5. **State Transitions**: Confirm rare state changes occur

### Patterns for Distributed Systems

**Good Patterns:**
```rust
// Failover scenarios
sometimes_assert!(
    failover_triggered,
    primary_failed && backup_activated,
    "Failover to backup should sometimes occur"
);

// Retry mechanisms
sometimes_assert!(
    retry_with_backoff,
    retry_count > 0 && backoff_applied,
    "Connection retries with backoff should sometimes happen"
);

// Queue saturation
sometimes_assert!(
    queue_full,
    queue.len() >= queue.capacity() * 0.9,
    "Message queue should sometimes approach capacity"
);

// Network issues
sometimes_assert!(
    connection_timeout,
    elapsed > timeout_threshold,
    "Connections should sometimes timeout"
);
```

**Anti-Patterns to Avoid:**
- Don't add sometimes assertions to trivial or always-executed paths
- Don't use them without understanding their purpose
- Don't add too many - be strategic and intentional
- Don't use for normal program flow validation

### Strategic Placement Guidelines

Place sometimes assertions in:
1. **Error handling blocks** - Verify error paths are tested
2. **Reconnection/retry logic** - Ensure resilience mechanisms activate
3. **Queue overflow handling** - Check backpressure scenarios
4. **Timeout handlers** - Validate timeout paths are exercised
5. **Concurrent access points** - Verify race conditions are explored
6. **Resource exhaustion handlers** - Test limit conditions
7. **Fallback mechanisms** - Ensure degraded modes are tested

### Key Principle
Sometimes assertions directly address testing completeness by ensuring that rare but important code paths are actually exercised during testing. They serve as a more nuanced alternative to traditional code coverage metrics by focusing on behavioral coverage rather than line coverage.