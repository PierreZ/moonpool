---
description: |
  Systematically find where to place assertions and buggify in moonpool Process/Workload code using 8 attention focuses.
  TRIGGER when: starting a new simulation, reviewing assertion coverage gaps, adding a new feature that needs property identification, or after debugging a seed failure to prevent similar bugs.
  DO NOT TRIGGER when: not working on moonpool simulation code.
---

# Discover Properties

## When to Use This Skill

Invoke when:
- Starting a new simulation and need to decide what properties to assert
- Reviewing an existing Process/Workload for assertion coverage gaps
- After adding a new feature and need to identify what could go wrong
- After debugging a seed failure and want to prevent similar bugs

## Quick Reference

| Focus | Looking for |
|-------|-------------|
| State Integrity | In-memory invariants, corruption paths, storage persistence gaps |
| Concurrency | Races between workloads, interleaving of async ops, shared mutable state |
| Crash Recovery | State lost on reboot, partial writes, recovery assumptions |
| Network Faults | Missing error handling, retry without idempotency, stale connections |
| Timing & Scheduling | Hardcoded timeouts, timer ordering assumptions, clock sensitivity |
| Resource Boundaries | Unbounded collections, missing backpressure, resource exhaustion |
| Protocol Contracts | Unenforced API guarantees, masked errors, ordering assumptions |
| Lifecycle Transitions | Requests before init, in-flight work on shutdown, premature state publish |

## Attention Focuses

### 1. State Integrity

Invariants on in-memory state, corruption paths, state that must survive reboots via `StorageProvider`.

**Look for**:
- Write ordering assumptions (write A before B, but what if crash between them?)
- State transitions that could be interrupted by process kill
- Reference model divergence (workload's expected state vs actual process state)
- Fields that should be monotonically increasing (ballot numbers, sequence IDs)
- Derived state that could become inconsistent with source data

### 2. Concurrency

Races between workloads hitting the same process, interleaving of async operations, shared mutable state.

**Look for**:
- Operations that assume sequential execution across `.await` points
- TOCTOU patterns: check a condition, then mutate based on it, with a yield in between
- Concurrent access to the same key/resource from multiple workloads
- `Rc<RefCell<>>` state accessed across await points where another task could mutate it

### 3. Crash Recovery

What happens when a Process is killed (graceful or crash) and restarted from factory.

**Look for**:
- In-memory state that isn't persisted to `StorageProvider` but should be
- Partially-written storage operations (write without sync, multi-file updates)
- Recovery code that assumes clean state (no torn writes, no partial data)
- Operations interrupted between storage write and `sync_all()`
- State that the workload expects to survive but lives only in process memory

### 4. Network Faults

Behavior under connection drops, partitions, reordering.

**Look for**:
- RPC calls without timeout or error handling
- Retry logic that isn't idempotent (retrying a non-idempotent operation after ambiguous failure)
- Stale connection state after partition restore (cached peer info, old leader references)
- Assumptions about message ordering (request A arrives before request B)
- Fire-and-forget sends where delivery matters

### 5. Timing & Scheduling

Sensitivity to event ordering, timer resolution, simulated clock behavior.

**Look for**:
- Hardcoded timeout values that interact poorly with other timeouts
- Operations that assume timers fire in a specific order relative to network events
- Races between timer expiry and data delivery
- Logic that depends on "enough time passing" without explicit synchronization
- Election/lease timeouts that could overlap

### 6. Resource Boundaries

Queue depths, connection limits, storage capacity.

**Look for**:
- Unbounded `Vec` or `VecDeque` that grow under load (message queues, request buffers)
- Missing backpressure on incoming requests
- Operations that assume resources are always available (connections, file handles)
- Memory growth patterns that only surface under sustained chaos

### 7. Protocol Contracts

API guarantees between Process and Workload, RPC contracts, wire format invariants.

**Look for**:
- Documented guarantees that aren't enforced with assertions
- Response codes that mask errors (returning OK when partially failed)
- Ordering assumptions between RPC calls (create before update)
- Request/response type mismatches that serde would silently accept
- Invariants claimed in doc comments but never tested

### 8. Lifecycle Transitions

Process startup, graceful shutdown, reboot sequences.

**Look for**:
- Requests arriving before initialization completes (listener bound before state loaded)
- In-flight work dropped during graceful shutdown (cancellation token not checked)
- State published to `StateRegistry` before it's valid
- Shutdown ordering dependencies (close connections before flushing storage)
- Factory assumptions about clean vs dirty process directories after wipe vs crash reboot

## Methodology

### Ensemble Mode (recommended)

When context allows, spawn one sub-agent per attention focus. Each agent examines the user's code through its assigned lens and returns:
- Suggested assertion placements with exact macro, location (`file:line`), and message
- Suggested buggify points with injection pattern and probability tier
- Confidence level (high/medium/low) and supporting evidence

Then synthesize: deduplicate (multiple agents finding the same property = high confidence), preserve unique finds (properties from only one focus are high-value catches), resolve conflicts, organize by file.

### Sequential Mode

Work through focuses as a checklist. Make an explicit pass for each. Do not skip a focus because an earlier pass "already covered" that area — the value is in the independent perspective.

## Output Format

For each discovered property:

```
[file_path:line_range] — Description

| Field     | Value                                                    |
|-----------|----------------------------------------------------------|
| Macro     | assert_always! / assert_sometimes! / assert_reachable!   |
| Message   | Unique, descriptive assertion message                    |
| Rationale | Why this property matters, what bug it catches           |
| Focus     | Which attention focus(es) surfaced this                  |
```

For buggify points:

```
[file_path:line_range] — Description

| Field       | Value                                                  |
|-------------|--------------------------------------------------------|
| Pattern     | Error injection / Delay / Parameter randomization      |
| Probability | High (5-10%) / Medium (1%) / Low (0.1-0.01%)          |
| Rationale   | What failure mode this exercises                       |
```

## Cross-References

- `using-assertions/SKILL.md` — Macro selection guidance (which assertion type to use)
- `using-buggify/SKILL.md` — Injection patterns and probability calibration
- `writing-a-process/SKILL.md` — Process lifecycle and factory patterns
- `writing-a-workload/SKILL.md` — Workload patterns and reference models

## Book Chapters

- `book/src/part3-building/24-discovering-properties.md` — Full guide: attention focuses, ensemble methodology, worked example
- `book/src/part3-building/12-assertions.md` — Assertion overview and taxonomy
- `book/src/part3-building/19-designing-workloads.md` — Invariant patterns for workloads
