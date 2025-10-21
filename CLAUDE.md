# Moonpool Workspace

Rust workspace containing two distinct projects:
- **moonpool-foundation**: A standalone deterministic simulation framework (inspired by FoundationDB)
- **moonpool**: A virtual actor system similar to Orleans

## What is Moonpool?

This workspace contains **two separate projects** with different purposes and completion status:

### moonpool-foundation (✅ COMPLETED - Standalone Framework)
A production-ready deterministic simulation framework inspired by FoundationDB's simulation testing approach:
- **Sans I/O transport layer** with request-response semantics
- **Comprehensive chaos testing** infrastructure (Buggify, sometimes assertions, invariant validation)
- **Provider pattern** for seamless simulation/production switching
- **Complete and usable independently** for testing any distributed system

### moonpool (❌ NOT BUILT YET - Actor System)
A planned virtual actor system similar to Microsoft Orleans:
- **Location transparency** - actors addressable by ID regardless of physical location
- **Automatic lifecycle management** - activation, deactivation, migration
- **MessageBus and ActorCatalog** - core actor runtime infrastructure
- **Currently in Phase 12 planning** - implementation not started

## Workspace Structure

```
moonpool/
├── moonpool-foundation/    # ✅ STANDALONE simulation framework (FDB-inspired, COMPLETED)
│   └── CLAUDE.md          # Foundation-specific development guide
├── moonpool/              # ❌ Virtual actor system (Orleans-like, NOT BUILT YET)
│   └── CLAUDE.md          # Actor system development guide (planning only)
└── docs/                  # Comprehensive documentation
    ├── specs/             # Technical specifications
    ├── plans/             # Phase-by-phase implementation plans
    ├── analysis/          # Reference architecture analysis (FDB, Orleans)
    └── references/        # Source code from FDB, Orleans, TigerBeetle
```

## Crate-Specific Instructions

- **Working on simulation/transport/networking?** → See `moonpool-foundation/CLAUDE.md`
- **Working on actors/MessageBus/ActorCatalog?** → See `moonpool/CLAUDE.md`

## Environment Setup

### Nix Development Environment
**Required**: All cargo commands must run within Nix shell

```bash
nix develop --command <cargo-command>
```

### Common Commands
```bash
# Format code
nix develop --command cargo fmt

# Check for issues
nix develop --command cargo clippy

# Run all tests
nix develop --command cargo nextest run

# Build workspace
nix develop --command cargo build

# Check specific crate
nix develop --command cargo check -p moonpool-foundation
nix develop --command cargo check -p moonpool
```

## Phase Completion Criteria

Before completing any phase or major feature:

1. **Code Quality**:
   - `cargo fmt` passes
   - `cargo clippy` produces no warnings
   - No `unwrap()` calls (use `Result` with `?`)
   - All public items documented

2. **Testing**:
   - `cargo nextest run` - all tests pass
   - No test timeouts or hangs
   - All `sometimes_assert!` triggered in chaos tests
   - 100% success rate across test iterations

3. **Validation**:
   - Full compilation (code + tests)
   - Multi-topology tests pass (1x1, 2x2, 10x10)
   - Invariants validated across workloads
   - Documentation updated

## Test Configuration

**Location**: `.config/nextest.toml`

**Timeouts**:
- Default: 1 second
- Slow simulation tests (name contains "slow_simulation"): 4 minutes

**Debug Testing**:
- Default: `UntilAllSometimesReached(1000)` for comprehensive chaos coverage
- Debug failing seeds: `FixedCount(1)` with specific seed and ERROR log level

## Git Workflow

### Creating Commits
Only commit when explicitly requested by the user.

**Before committing**:
1. Run `git status` to see untracked files
2. Run `git diff` to see changes
3. Run `git log` to check commit message style
4. Analyze changes and draft message

**Commit Message Format**:
```
<type>: <concise summary>

<optional detailed description>
```

**Types**: feat, fix, docs, test, refactor, chore

**Important**:
- Focus on "why" rather than "what"
- DO NOT push unless explicitly requested
- Use HEREDOC for multi-line messages
- Never skip hooks (no --no-verify)

## Cross-Cutting Constraints

Apply to all crates in workspace:

- **Single-core execution**: No Send/Sync requirements
- **No unwrap()**: Use `Result<T, E>` with `?` operator
- **Document public APIs**: All public items need docs
- **Async traits**: Use `#[async_trait(?Send)]` for networking
- **Trait-based design**: Depend on traits, not concrete types
- **KISS principle**: Simplicity over features
- **Provider pattern**: Use TimeProvider, NetworkProvider, TaskProvider traits
- **State machines**: Use explicit enum-based state machines for protocols and complex lifecycle management

### State Machine Pattern

For any protocol implementation or component with complex lifecycle, use explicit enum-based state machines.

**Benefits**:
- **Type safety**: Invalid state transitions become compile errors
- **Clarity**: All possible states are explicit and documented
- **Testability**: Easy to test specific state transitions
- **Debuggability**: Clear current state in logs and debugging

**When to use**:
- Network protocols (connection lifecycle, request-response)
- Actor lifecycle (activation, deactivation, migration)
- Multi-step workflows (cluster formation, leader election)
- Any component with 3+ distinct states

**Avoid**: Boolean soup (multiple boolean flags where valid combinations are unclear)

### Forbidden Patterns
❌ `tokio::time::sleep()` → ✅ `time.sleep()`
❌ `tokio::time::timeout()` → ✅ `time.timeout()`
❌ `tokio::spawn()` → ✅ `task_provider.spawn_task()`
❌ `unwrap()` / `expect()` → ✅ `?` operator
❌ `LocalSet` → ✅ `Builder::new_current_thread().build_local()`

## Documentation Index

Comprehensive documentation organized by purpose:

### Specifications (`docs/specs/`)
Technical architecture and design documents:
- `moonpool-foundation-spec.md` - Framework overview
- `simulation-core-spec.md` - Core simulation infrastructure
- `transport-layer-spec.md` - Sans I/O transport layer
- `peer-networking-spec.md` - TCP connection management
- `testing-framework-spec.md` - Chaos testing and assertions

### Implementation Plans (`docs/plans/`)
Phase-by-phase roadmaps (see `docs/INDEX.md` for complete listing):
- Phases 1-11: Foundation layer (COMPLETED)
- Phase 12+: Actor system (IN PROGRESS)

### Analysis Documents (`docs/analysis/`)
Deep dives into reference architectures:
- `foundationdb/flow.md` - **READ FIRST** before touching actor.cpp
- `foundationdb/fdb-network.md` - Network architecture
- `orleans/` - Actor system patterns

### Reference Code (`docs/references/`)
Source code from production systems:
- `foundationdb/` - Simulation, networking, chaos testing
- `orleans/` - Actor system implementation
- `tigerbeetle/` - Packet simulation

**See `docs/INDEX.md` for complete file listing with descriptions**

## Current Status

### moonpool-foundation: ✅ COMPLETE & PRODUCTION-READY
**Standalone deterministic simulation framework** (inspired by FoundationDB):
- Phases 1-11 fully implemented and tested
- Sans I/O transport layer with request-response semantics
- Comprehensive chaos testing infrastructure
- **Can be used independently** for testing any distributed system
- All tests passing, 100% coverage of sometimes assertions

### moonpool: ❌ NOT BUILT YET
**Virtual actor system** (similar to Microsoft Orleans):
- Phase 12+ planning in progress
- **No code implementation yet** - only design documents and analysis
- Will build on moonpool-foundation when ready
- See `docs/analysis/orleans/` for reference architecture research

## Testing Philosophy

**Goal**: Find bugs before production through hostile infrastructure simulation

- **Deterministic**: Same seed = identical behavior
- **Chaos by default**: 10x worse than production conditions
- **Comprehensive coverage**: All error paths tested via `sometimes_assert!`
- **Invariant validation**: Cross-workload properties checked after every event
- **100% success rate**: No deadlocks or hangs acceptable

## Getting Help

- **Foundation questions?** → Read `moonpool-foundation/CLAUDE.md` and `docs/specs/`
- **Actor system questions?** → Read `moonpool/CLAUDE.md` and `docs/analysis/orleans/`
- **Architecture questions?** → Start with `docs/INDEX.md`
- **Reference implementations?** → Check `docs/references/` and analysis docs
