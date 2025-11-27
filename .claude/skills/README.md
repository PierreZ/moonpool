# Moonpool Chaos Testing Skills

This directory contains specialized skills for deterministic chaos testing in the moonpool framework.

## Overview

Chaos testing in moonpool follows FoundationDB's simulation approach: run production code in a simulated environment with deterministic fault injection to find bugs before they reach production.

**Four focused skills**:
1. **designing-simulation-workloads** - Create autonomous tests that explore state space
2. **using-buggify** - Inject faults strategically to force edge cases
3. **using-chaos-assertions** - Track coverage and validate safety properties
4. **validating-with-invariants** - Check global system properties

## Quick Start

### Learning Path

**Recommended order**:
1. Start with `designing-simulation-workloads` to understand autonomous testing philosophy
2. Add `using-buggify` to inject chaos into your workload
3. Use `using-chaos-assertions` to track coverage and validate properties
4. Apply `validating-with-invariants` for cross-actor validation

### First-Time Users

If new to chaos testing:
```
Read: designing-simulation-workloads/SKILL.md
Goal: Understand Plinko board mental model and operation alphabets

Then: using-chaos-assertions/SKILL.md
Goal: Learn sometimes_assert! vs always_assert!

Then: using-buggify/SKILL.md
Goal: Add fault injection to force error paths

Finally: validating-with-invariants/SKILL.md
Goal: Add global invariant checks
```

### Existing Test Enhancement

If you have tests but want better coverage:
```
Start: using-buggify/SKILL.md
Goal: Add buggify to existing tests

Then: using-chaos-assertions/SKILL.md
Goal: Add assertions to track coverage gaps

Then: designing-simulation-workloads/SKILL.md
Goal: Expand operation alphabet for deeper exploration
```

## When to Use Each Skill

### Use "designing-simulation-workloads" When:
- ✓ Creating new simulation tests
- ✓ Designing operation alphabets
- ✓ Planning verification strategies
- ✓ Scaling tests from 1x1 to 10x10 topology
- ✓ Workload isn't finding enough bugs

**Invoke with**: "Help me design a simulation workload for [component]"

### Use "using-buggify" When:
- ✓ Adding fault injection to code
- ✓ Debugging why simulation doesn't catch specific bugs
- ✓ Forcing rare error paths to execute
- ✓ Creating time pressure with shortened timeouts
- ✓ Buggify isn't triggering expected paths

**Invoke with**: "Help me add buggify to [component] for [error scenario]"

### Use "using-chaos-assertions" When:
- ✓ Adding coverage tracking to tests
- ✓ Validating safety properties
- ✓ Analyzing coverage gaps (0% assertions)
- ✓ Reproducing failures from specific seeds
- ✓ Understanding why certain paths aren't tested

**Invoke with**: "Help me add assertions to track [coverage scenario]"

### Use "validating-with-invariants" When:
- ✓ Designing cross-actor properties
- ✓ Implementing global system constraints
- ✓ Setting up the JSON-based invariant system
- ✓ Finding distributed correctness bugs
- ✓ Need validation beyond per-actor assertions

**Invoke with**: "Help me design invariants for [distributed property]"

## Decision Flowchart

```
Starting new simulation test?
│
├─ YES → Use "designing-simulation-workloads"
│        Create operation alphabet + workload structure
│        │
│        ↓
│        Add fault injection?
│        │
│        └─ YES → Use "using-buggify"
│                 Place buggify strategically
│                 │
│                 ↓
│                 Track coverage?
│                 │
│                 └─ YES → Use "using-chaos-assertions"
│                          Add sometimes_assert! + always_assert!
│                          │
│                          ↓
│                          Need global checks?
│                          │
│                          └─ YES → Use "validating-with-invariants"
│                                   Design cross-actor invariants
│
└─ NO → Enhancing existing test?
        │
        ├─ Coverage gaps? → Use "using-chaos-assertions"
        │                    Analyze 0% assertions, fix gaps
        │
        ├─ Missing error paths? → Use "using-buggify"
        │                          Add strategic fault injection
        │
        ├─ Need more scenarios? → Use "designing-simulation-workloads"
        │                          Expand operation alphabet
        │
        └─ Distributed bugs? → Use "validating-with-invariants"
                                Add cross-actor checks
```

## How Skills Complement Each Other

### Skill Interactions

```
┌─────────────────────────────┐
│ designing-simulation-       │
│ workloads                   │◄──────┐
│                             │       │
│ • Operation alphabet        │       │
│ • Workload structure        │       │ Informs
│ • Topology scaling          │       │ design
└─────────┬───────────────────┘       │
          │                           │
          │ Defines operations        │
          │ that use...               │
          ↓                           │
┌─────────────────────────────┐       │
│ using-buggify               │       │
│                             │       │
│ • Strategic placement       │       │
│ • Fault injection           │       │
│ • Chaos creation            │       │
└─────────┬───────────────────┘       │
          │                           │
          │ Triggers paths            │
          │ tracked by...             │
          ↓                           │
┌─────────────────────────────┐       │
│ using-chaos-assertions      │       │
│                             │       │
│ • Coverage tracking         │───────┘ Feedback loop:
│ • Safety validation         │       gaps drive
│ • Gap identification        │       more ops
└─────────┬───────────────────┘
          │
          │ Per-actor checks
          │ complement...
          ↓
┌─────────────────────────────┐
│ validating-with-invariants  │
│                             │
│ • Cross-actor properties    │
│ • Global consistency        │
│ • System-wide validation    │
└─────────────────────────────┘
```

### Example Workflow

**Scenario**: Testing actor activation

1. **designing-simulation-workloads**: Create operation alphabet
   ```rust
   enum Op {
       ActivateActor(ActorId),
       DeactivateActor(ActorId),
       SendMessage(ActorId, Msg),
   }
   ```

2. **using-buggify**: Add fault injection
   ```rust
   if buggify!() {
       // Widen race window
       time.sleep(Duration::from_millis(50)).await;
   }
   ```

3. **using-chaos-assertions**: Track coverage
   ```rust
   sometimes_assert!(
       concurrent_activation_handled,
       true,
       "Concurrent activation detected"
   );
   ```

4. **validating-with-invariants**: Global check
   ```rust
   fn check_single_activation(registry: &StateRegistry) -> Result<(), String> {
       // Verify no actor activated twice across all nodes
   }
   ```

## Skill Reference

### designing-simulation-workloads
- **SKILL.md**: Main concepts, Plinko mental model, four principles
- **EXAMPLES.md**: Bank Account, Directory, MessageBus workloads
- **PATTERNS.md**: Operation alphabet implementations
- **VERIFICATION.md**: Three verification patterns

### using-buggify
- **SKILL.md**: Strategic placement, probability tuning
- **PLACEMENT-GUIDE.md**: Decision tree, code location analysis
- **EXAMPLES.md**: Real-world examples from moonpool-foundation
- **TROUBLESHOOTING.md**: Common issues and solutions

### using-chaos-assertions
- **SKILL.md**: sometimes_assert! vs always_assert!, coverage validation
- **ASSERTION-PATTERNS.md**: Common patterns by use case
- **COVERAGE-ANALYSIS.md**: Interpreting reports, fixing gaps
- **EXAMPLES.md**: Real assertions from ping_pong tests

### validating-with-invariants
- **SKILL.md**: Invariant thinking, when to use, JSON-based system
- **REFERENCE.md**: StateRegistry API, check function signatures
- **EXAMPLES.md**: Conservation laws, uniqueness constraints

## Common Questions

### Q: Which skill should I start with?
**A**: `designing-simulation-workloads` for new tests, `using-chaos-assertions` for analyzing existing tests.

### Q: Do I need all four skills?
**A**: Not always. Small systems might only need workloads + assertions. Distributed systems benefit from all four.

### Q: When should I add invariants vs assertions?
**A**: Assertions for per-actor properties, invariants for cross-actor properties. See `validating-with-invariants/SKILL.md`.

### Q: My simulation isn't finding bugs. Which skill?
**A**: Start with `using-chaos-assertions` to analyze coverage gaps, then `using-buggify` to add missing fault injection.

### Q: How do I reproduce a failing seed?
**A**: See seed reproduction workflow in `using-chaos-assertions/SKILL.md`.

### Q: Tests are too slow. Which skill has performance tips?
**A**: `using-buggify/SKILL.md` (Performance Considerations section) and `validating-with-invariants/SKILL.md` (efficient checks).

## Philosophy Summary

**Core Principles** (across all skills):
1. **Deterministic execution** - Same seed = same behavior
2. **Autonomous exploration** - System explores state space, not hand-crafted scenarios
3. **Coverage tracking** - Prove every path tested
4. **Safety validation** - Properties never violated
5. **Chaos by default** - 10x worse than production

**Inspired by FoundationDB**: Same production code runs in simulation with swappable interfaces (INetwork → Net2 vs Sim2).

## Getting Help

If stuck:
1. Check relevant skill's SKILL.md for concepts
2. Review reference files for examples
3. Search for similar patterns in EXAMPLES.md files
4. Check TROUBLESHOOTING.md (in using-buggify skill)

## Contributing

When adding new patterns or examples:
- Keep SKILL.md under 500 lines (use reference files for details)
- Use gerund naming (verb + -ing)
- Include "when to use" triggers
- Cross-reference related skills
- Test examples before documenting

---

**Remember**: The goal is to find bugs during development through systematic chaos, not regression testing!

"Never been woken up by FDB" - because simulation encounters every failure mode first.
