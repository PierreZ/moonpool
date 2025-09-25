# Phase 1: Core Infrastructure Implementation Plan

## Overview

Phase 1 establishes the foundation of the Moonpool simulation framework with deterministic time, event processing, random number generation, and basic simulation harness. This phase creates the core abstractions that all subsequent phases will build upon.

## Goals

- Deterministic, reproducible simulation execution
- Logical time advancement through event processing
- Seed-based randomness for consistent behavior
- Basic simulation world coordination
- Foundation for network and I/O simulation

## Implementation Tasks

### 1. Event Queue and Processing

**File**: `src/events.rs`

**Core Components:**
- `Event`: Enum representing different event types
- `EventQueue`: Priority queue ordered by scheduled time
- Event processing loop with deterministic ordering

**Implementation Details:**
- Binary heap for O(log n) event scheduling and retrieval
- Events scheduled with precise timestamps
- Deterministic processing order for events at the same time
- Support for immediate event scheduling

**Key APIs:**
```rust
pub enum Event {
    Wake { task_id: u64 },
    // More event types in later phases
}

pub struct EventQueue {
    heap: BinaryHeap<ScheduledEvent>,
}

pub struct ScheduledEvent {
    time: Duration, // Use Duration directly for simplicity
    event: Event,
    sequence: u64, // For deterministic ordering
}
```

### 2. Basic Simulation Harness (`SimWorld`)

**File**: `src/sim.rs`

**Core Components:**
- `SimWorld`: Central coordination struct owning all mutable state
- `WeakSimWorld`: Weak reference handle for accessing simulation state
- Basic handle pattern implementation

**Implementation Details:**
- Centralized ownership model as described in spec
- Handle-based access pattern to avoid borrow checker issues
- Integration of event queue with simple time tracking
- Time starts at Duration::ZERO (representing 0 unix time)
- Basic simulation lifecycle management

**Key APIs:**
```rust
pub struct SimWorld {
    inner: Rc<RefCell<SimInner>>,
}

struct SimInner {
    current_time: Duration, // Starts at Duration::ZERO (0 unix time)
    event_queue: EventQueue,
    next_sequence: u64,
}

pub struct WeakSimWorld {
    inner: Weak<RefCell<SimInner>>,
}

impl SimWorld {
    pub fn new() -> Self;
    pub fn step(&mut self) -> bool; // Process one event, advance time, return if more events
    pub fn run_until_empty(&mut self);
    pub fn current_time(&self) -> Duration;
    pub fn schedule_event(&self, event: Event, delay: Duration);
    pub fn downgrade(&self) -> WeakSimWorld;
}
```

## Module Structure

```
moonpool-foundation/src/
├── lib.rs              # Public API exports
├── sim.rs              # Central SimWorld coordination
├── events.rs           # Event queue and processing
└── error.rs            # Error types
```

## Testing Strategy

### Unit Tests
- Event queue ordering and processing
- Time advancement correctness
- Handle lifecycle management

### Integration Tests  
- End-to-end simulation execution
- Event processing order verification

### Deterministic Tests
- **Same-time events**: Events scheduled at identical times must process in deterministic order using sequence numbers
- **Multiple runs**: Same sequence of event scheduling must produce identical execution order across runs
- **Interleaved scheduling**: Events scheduled in different orders but with same timestamps must still execute deterministically

### Test Files
- `tests/event_tests.rs` 
- `tests/sim_tests.rs`
- `tests/determinism_tests.rs` - Focused on deterministic behavior verification

## Dependencies

**Required Crates:**
```toml
[dependencies]
# No external dependencies needed for Phase 1

[dev-dependencies]
tokio = { version = "1.0", features = ["macros", "rt"] }
```

## Success Criteria

Phase 1 is complete when:

1. ✅ **Event Queue**: Priority queue with deterministic ordering works correctly
2. ✅ **Time Advancement**: Time advances only when events are processed
3. ✅ **Basic Coordination**: SimWorld struct coordinates time and events
4. ✅ **Handle Pattern**: WeakSimWorld handles work without borrowing conflicts
5. ✅ **Deterministic Behavior**: Events at same timestamp process in consistent order across runs
6. ✅ **Test Coverage**: Tests verify event processing, time advancement, and deterministic behavior

## Implementation Order

1. **Event Queue** - Core event scheduling and processing
2. **SimWorld Harness** - Coordinate time and events
3. **Integration** - Wire everything together
4. **Testing** - Verify deterministic behavior

## Future Integration Points

Phase 1 creates the foundation that Phase 2 will extend:

- Event types will expand to include network messages
- Additional components (RNG, TimeProvider, NetworkEngine) will integrate with the existing SimWorld/EventQueue foundation
- SimWorld will coordinate additional state alongside time/events

The clean abstractions in Phase 1 ensure that additional features can be added without major refactoring of the core infrastructure.