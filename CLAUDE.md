# Claude Context for Moonpool

## Project Overview
Moonpool is a Rust project designed to create the right toolbox for building distributed systems. The project is structured as a workspace with multiple components.

## Current Focus: Simulation Framework
Currently working on the simulation framework located in `moonpool-simulation`. This is a deterministic simulation framework for testing distributed systems.

## Development Environment Setup

**IMPORTANT**: Claude must use `nix develop` shell for compilation and testing.

### For Claude Code
When running Rust commands (cargo build, cargo test, etc.), Claude MUST use:
```bash
nix develop --command <cargo-command>
```

Example usage:
- `nix develop --command cargo test` - Run tests
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
cargo test   # Then run commands normally
```

## Phase Completion Criteria

**IMPORTANT**: A phase is considered finished only when ALL of the following pass:

```bash
nix develop --command cargo fmt    # Code formatting
nix develop --command cargo clippy # Linting and code quality
nix develop --command cargo test   # All tests pass
```

### Phase Completion Checklist
- ✅ **cargo fmt** - Code is properly formatted
- ✅ **cargo clippy** - No linting warnings or errors  
- ✅ **cargo test** - All tests pass (unit, integration, doc tests)
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

**Do not consider a phase complete until all three cargo commands pass without warnings or errors.**

## Current Task
- ✅ Phase 1 implementation completed with full test coverage
- Ready for Phase 2: Network simulation features

## Project Structure
- `moonpool-simulation/` - Main simulation framework implementation (Phase 1 complete)
- `docs/specs/simulation/` - Simulation framework specification
- `docs/plans/` - Implementation plans and roadmaps
- `docs/references/` - Reference code from other projects for inspiration

## Phase 1 Status: ✅ COMPLETED

### Phase Completion Verification
- ✅ **cargo fmt** - Code properly formatted (no changes needed)
- ✅ **cargo clippy** - No linting warnings or errors
- ✅ **cargo test** - All 14 tests passing (8 unit + 5 integration + 1 doc test)

### Implemented Features
- Event queue with deterministic ordering
- Logical time advancement engine
- Basic simulation harness with handle pattern
- Comprehensive test coverage including deterministic behavior verification
- thiserror integration for better error handling
- Clean module structure with documented public API

**Phase 1 meets all completion criteria and is ready for Phase 2 development.**