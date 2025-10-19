# Moonpool Workspace

Rust workspace for deterministic distributed systems testing and actor-based programming.

## Workspace Structure

```
moonpool/
├── moonpool-foundation/    # Core simulation & transport layer (Phases 1-11 COMPLETED)
│   └── CLAUDE.md          # Foundation-specific development guide
├── moonpool/              # Actor system (Phase 12+ IN PROGRESS)
│   └── CLAUDE.md          # Actor system development guide
└── docs/                  # Comprehensive documentation
    ├── specs/             # Technical specifications
    ├── plans/             # Phase-by-phase implementation plans
    ├── analysis/          # Reference architecture analysis
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

- ✅ **moonpool-foundation** (Phases 1-11): Core simulation framework with Sans I/O transport layer
- 🚧 **moonpool** (Phase 12+): Actor system implementation beginning

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
