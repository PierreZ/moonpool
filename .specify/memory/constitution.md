<!--
Sync Impact Report - Constitution Update
=========================================
Version Change: [NEW] → 1.0.0
Initial constitution ratification for Moonpool project

Modified Principles: N/A (initial version)
Added Sections:
  - Core Principles (7 principles)
  - Testing & Quality Standards
  - Development Workflow
  - Governance

Templates Status:
  ✅ plan-template.md - No updates needed (Constitution Check section already generic)
  ✅ spec-template.md - No updates needed (requirements-focused, no principle conflicts)
  ✅ tasks-template.md - No updates needed (test discipline already reflected)
  ✅ Command files - No updates needed (no agent-specific references found)

Follow-up TODOs: None
-->

# Moonpool Constitution

This constitution establishes the non-negotiable principles and practices governing all development work across the Moonpool workspace (both moonpool-foundation and the planned moonpool actor system).

## Core Principles

### I. Determinism Over Convenience

**All async operations MUST use provider abstractions, never direct runtime calls.**

Rationale: Deterministic simulation is the foundation of our testing philosophy. Any direct use of `tokio::time`, `tokio::spawn`, or other runtime primitives breaks determinism and invalidates our ability to reproduce bugs through seed-based testing.

Rules:
- MUST use `TimeProvider::sleep()` instead of `tokio::time::sleep()`
- MUST use `TimeProvider::timeout()` instead of `tokio::time::timeout()`
- MUST use `TaskProvider::spawn_task()` instead of `tokio::spawn()`
- MUST use provider traits (TimeProvider, NetworkProvider, TaskProvider, RandomProvider) for all I/O
- Production mode uses real implementations; simulation mode uses deterministic implementations
- Same seed MUST produce identical behavior for bug reproduction

### II. Explicitness Over Implicitness

**All error handling MUST be explicit; unwrap() and expect() are forbidden in production code.**

Rationale: Silent failures and panics are unacceptable in distributed systems. Explicit error propagation with the `?` operator forces handling at appropriate levels and creates clear error paths that can be tested.

Rules:
- MUST use `Result<T, E>` for fallible operations
- MUST propagate errors with `?` operator
- NO `unwrap()` or `expect()` calls (forbidden pattern)
- Error types MUST be descriptive and carry context
- Test code may use `unwrap()` only when failure represents a test bug, not a code path being tested

### III. Single-Core Simplicity

**No Send/Sync requirements; all code runs on a single thread.**

Rationale: Distributed systems complexity lies in coordination across machines, not threads. Single-threaded execution eliminates data races, lock contention, and thread-safety concerns, allowing focus on distributed protocols and simulation correctness.

Rules:
- Use `#[async_trait(?Send)]` for all async traits
- Use `Builder::new_current_thread().build_local()` for Tokio runtime
- NO `LocalSet` wrapper (forbidden pattern)
- NO thread-safety primitives (Mutex, RwLock, Arc) except at network boundaries
- Simulation and production modes both respect single-thread constraint

### IV. Trait-Based Design

**Depend on traits, not concrete types.**

Rationale: The provider pattern and seamless simulation/production switching require loose coupling. Trait-based design enables testing, mocking, and runtime polymorphism without dynamic dispatch overhead through monomorphization.

Rules:
- Public APIs MUST accept trait bounds, not concrete types
- Core abstractions (Network, Time, Task, Random) MUST be traits
- Implementations MAY be concrete, but interfaces MUST be abstract
- Use `impl Trait` or generic bounds for parameters
- Prefer composition over inheritance

### V. State Machine Clarity

**Complex lifecycles MUST use explicit enum-based state machines, not boolean soup.**

Rationale: Protocols and actor lifecycles have multiple states with specific valid transitions. Explicit state machines make invalid states unrepresentable, provide compile-time safety, and create self-documenting code.

Rules:
- MUST use state machine pattern for:
  - Network protocols (connection lifecycle, request-response)
  - Actor lifecycle (activation, deactivation, migration)
  - Multi-step workflows (cluster formation, leader election)
  - Any component with 3+ distinct states
- State transitions MUST be explicit and documented
- Invalid states MUST be unrepresentable at type level
- Avoid boolean flags where state combinations are unclear

### VI. Comprehensive Chaos Testing

**All features MUST be tested under hostile simulation conditions with 100% sometimes assertion coverage.**

Rationale: Distributed systems bugs are rare, timing-dependent, and catastrophic. Chaos testing with conditions 10x worse than production exposes edge cases before deployment. The `sometimes_assert!` system ensures all code paths execute across deterministic test runs.

Rules:
- Every feature MUST have simulation tests with Buggify enabled
- `always_assert!` for invariants that MUST never be violated
- `sometimes_assert!` for behaviors that should occur under normal conditions
- Target: 100% coverage of all `sometimes_assert!` across multi-seed runs
- Cross-workload invariants MUST validate global properties after every simulation event
- Multi-topology tests (1x1, 2x2, 10x10) MUST pass before phase completion
- Hostile infrastructure: delays, packet loss, reordering 10x worse than production
- No deadlocks or test hangs acceptable (strict timeout enforcement)

### VII. Simplicity First (KISS Principle)

**Start simple, add complexity only when justified.**

Rationale: Over-engineering is the enemy of correctness and maintainability. Simple solutions are easier to test, debug, and reason about. Complexity must be justified by clear requirements, not anticipated future needs (YAGNI).

Rules:
- Prefer simple, direct implementations over clever abstractions
- Add features/complexity only when current implementation provably insufficient
- Document justification for any complexity in plan.md Complexity Tracking table
- Avoid premature optimization
- Delete unused code aggressively
- Favor readability over terseness

## Testing & Quality Standards

### Phase Completion Criteria

**GATE: All phases MUST meet these criteria before considered complete.**

Before completing any phase or major feature:

1. **Code Quality**:
   - `cargo fmt` passes (formatting enforced)
   - `cargo clippy` produces no warnings (linting clean)
   - No `unwrap()` / `expect()` calls in production code
   - All public items documented with `///` doc comments

2. **Testing**:
   - `cargo nextest run` - all tests pass
   - No test timeouts or hangs
   - All `sometimes_assert!` triggered across multi-seed runs
   - 100% success rate across test iterations
   - Multi-topology tests pass (1x1, 2x2, 10x10 where applicable)

3. **Validation**:
   - Full compilation (code + tests)
   - Invariants validated across all workloads
   - Documentation updated (specs, plans, CLAUDE.md)

### Test Configuration

- Location: `.config/nextest.toml`
- Default timeout: 1 second
- Slow simulation tests (name contains "slow_simulation"): 4 minutes
- Chaos mode default: `UntilAllSometimesReached(1000)` for comprehensive coverage
- Debug mode: `FixedCount(1)` with specific seed for bug reproduction

### Documentation Requirements

All public items (modules, structs, enums, traits, functions) MUST have:
- Purpose: What it does
- Rationale: Why it exists (if not obvious)
- Invariants: What must always be true
- Examples: How to use (for complex APIs)

## Development Workflow

### Nix Environment

**All cargo commands MUST run within Nix development environment:**

```bash
nix develop --command cargo <subcommand>
```

This ensures reproducible builds and consistent tooling across developers.

### Git Commit Discipline

**Only commit when explicitly requested by user.**

Commit message format:
```
<type>: <concise summary>

<optional detailed description>
```

Types: `feat`, `fix`, `docs`, `test`, `refactor`, `chore`

Before committing:
1. Run `git status` to see untracked files
2. Run `git diff` to see changes
3. Run `git log` to check commit message style
4. Analyze changes and draft message

Important:
- Focus on "why" rather than "what"
- DO NOT push unless explicitly requested
- Use HEREDOC for multi-line messages
- Never skip hooks (no `--no-verify`)

### Forbidden Patterns

These patterns violate our core principles and MUST NOT be used:

- ❌ `tokio::time::sleep()` → ✅ `time.sleep()`
- ❌ `tokio::time::timeout()` → ✅ `time.timeout()`
- ❌ `tokio::spawn()` → ✅ `task_provider.spawn_task()`
- ❌ `unwrap()` / `expect()` → ✅ `?` operator
- ❌ `LocalSet` → ✅ `Builder::new_current_thread().build_local()`
- ❌ Boolean soup → ✅ Explicit enum state machines

## Governance

### Amendment Process

This constitution supersedes all other practices and documentation. Amendments require:

1. **Proposal**: Document the proposed change and rationale
2. **Impact Analysis**: Identify affected templates, specs, and code
3. **Version Bump**: Follow semantic versioning
   - MAJOR: Backward incompatible governance/principle removals or redefinitions
   - MINOR: New principle/section added or materially expanded guidance
   - PATCH: Clarifications, wording, typo fixes, non-semantic refinements
4. **Template Sync**: Update all dependent templates (plan, spec, tasks, commands)
5. **Migration Plan**: Document how to bring existing code into compliance

### Compliance Review

All PRs and design reviews MUST verify compliance with this constitution:
- Do provider abstractions respect Determinism Over Convenience?
- Are errors handled explicitly per Explicitness Over Implicitness?
- Does state management use clear machines per State Machine Clarity?
- Are tests comprehensive per Comprehensive Chaos Testing?
- Is complexity justified per Simplicity First?

Violations MUST be justified in plan.md Complexity Tracking table with:
- What principle is violated
- Why it's needed for this specific feature
- What simpler alternative was rejected and why

### Runtime Development Guidance

**This constitution establishes WHAT principles govern the project.**

**For HOW to apply these principles during development**, see:
- Workspace root: `CLAUDE.md` (cross-cutting constraints and workflow)
- moonpool-foundation: `moonpool-foundation/CLAUDE.md` (simulation/transport specifics)
- moonpool actor system: `moonpool/CLAUDE.md` (actor system specifics)

The CLAUDE.md files provide tactical guidance that MUST align with these strategic principles.

**Version**: 1.0.0 | **Ratified**: 2025-10-21 | **Last Amended**: 2025-10-21
