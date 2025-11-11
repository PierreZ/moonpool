# MessageBus Simulation Testing - Incremental Plan

**Goal**: Add deterministic chaos testing to the MessageBus using moonpool-foundation's simulation framework

**Strategy**: Start with minimal I/O-enabled test, incrementally add complexity and chaos

---

## Phase 1: Enhanced MessageBus with Autonomous Testing

**Duration**: ~6 hours
**Status**: 🚧 IN PROGRESS (90% complete - network active, debugging response routing)

### Objectives
- Create comprehensive MessageBus simulation test with real ActorRuntime integration
- Implement autonomous testing pattern (operation generator)
- Add strategic chaos injection (buggify) throughout MessageBus code
- Establish comprehensive coverage assertions (15-20 sometimes_assert)
- Validate message conservation and correlation tracking invariants

### Implementation - Enhanced Approach
- **Files**:
  - `moonpool/tests/simulation/minimal_message_bus.rs` - Main test with autonomous workload
  - `moonpool/tests/simulation/actors/ping_pong_actor.rs` - Test actor for request-response
  - `moonpool/src/messaging/bus.rs` - Added 6 strategic buggify calls + 11 coverage assertions
- **Setup**: 2 nodes (client + server) with ActorRuntime, PingPongActor for request-response
- **Workload**: Autonomous operation generator producing 100-500 operations per run
- **Operation Alphabet**:
  ```rust
  enum MessageBusOp {
      SendPing { target_actor, timeout_ms },  // 80% - normal ping
      Wait { duration_ms },                    // 20% - timing variation
      BurstSend { target_actor, count, timeout_ms }, // 5% - stress test
  }
  ```
- **Buggify Locations** (6 strategic placements):
  1. `bus.rs:685` - Race window in directory lookup
  2. `bus.rs:712` - Network send failure (25% prob when active)
  3. `bus.rs:763` - Slow node load query
  4. `bus.rs:813` - Placement forward network failure (20% prob)
  5. `bus.rs:876` - Local routing timing variation
  6. `bus.rs:925` - Callback delivery timing variation

- **Coverage Assertions** (11 in MessageBus):
  - `messagebus_remote_forward` - Message forwarded to remote node
  - `messagebus_directory_hit_local` - Actor found in directory (local)
  - `messagebus_directory_miss` - Directory miss triggers placement
  - `messagebus_placement_consulted` - Placement strategy invoked
  - `messagebus_placement_chose_remote` - Placement selected remote node
  - `messagebus_placement_chose_local` - Placement selected local node
  - `messagebus_directory_error` - Directory lookup failed
  - `messagebus_local_routing` - Local routing performed
  - `messagebus_response_routed` - Response routed to callback
  - `messagebus_callback_completed` - Callback delivery completed
  - Plus 7 in test workload (client operations)

- **Comprehensive Invariants**:
  1. **Message Conservation**: `sent = received + in_transit + timeouts` (strict accounting)
  2. **No Correlation Leaks**: All pending requests resolved at completion
  3. **Completion Tracking**: All nodes reach completed status

### Success Criteria
- [x] ActorRuntime integrated with simulation providers
- [x] PingPongActor test infrastructure created
- [x] Autonomous operation generator implemented
- [x] 6 strategic buggify calls added to MessageBus
- [x] 11 coverage assertions in MessageBus code
- [x] 3 comprehensive invariants (conservation, leaks, completion)
- [x] Test compiles and runs successfully
- [x] Network layer active (foundation assertions triggering)
- [ ] Response routing working (currently 0 responses received)
- [ ] 100% sometimes_assert coverage across 1000+ seeds
- [ ] All invariants hold (no violations)
- [ ] 100% success rate (no failing seeds)

### What Was Accomplished
- ✅ Created PingPongActor with message handlers and state management
- ✅ Integrated ActorRuntime with simulation providers (NetworkProvider, TimeProvider, TaskProvider)
- ✅ Implemented autonomous operation generator with 3 operation types
- ✅ Added 6 strategic buggify calls throughout MessageBus routing logic
- ✅ Added 11 coverage assertions to track all MessageBus code paths
- ✅ Implemented 3 comprehensive invariants with detailed validation
- ✅ Fixed foundation ergonomics: `always_assert!` now supports format args, added `RandomProvider::choice()`
- ✅ Fixed ActorRuntime simulation setup:
  - Added shared Directory and Storage across workloads
  - Added `clear()` methods for per-seed state cleanup
  - Configured cluster_nodes, storage, and shared_directory
  - Added 1-second startup delay (mirrors hello_actor example)
  - Enabled Tokio runtime time/IO features
- 🚧 Debugging response routing (218 sent, 0 received, 16 timeouts)

### Foundation Ergonomics Improvements (Completed)
1. **Enhanced `always_assert!` macro**: Now supports format arguments like standard `assert!`
   ```rust
   // Before: always_assert!(name, cond, format!("val: {}", x))
   // After:  always_assert!(name, cond, "val: {}", x)
   ```
2. **Added `RandomProvider::choice()` method**: Common pattern now built-in
   ```rust
   // Before: let idx = random.random_range(0..vec.len()); let item = &vec[idx];
   // After:  let item = random.choice(&vec);
   ```

### Key Learnings
1. **Autonomous testing is powerful**: Operation generator with 100-500 ops finds edge cases automatically
2. **Strategic buggify placement**: Focus on state transitions, error paths, timing-sensitive code
3. **Comprehensive assertions essential**: Need 15-20 to achieve full coverage of code paths
4. **Invariants catch what assertions miss**: Message conservation found accounting bugs
5. **Result nesting complexity**: `timeout()` returns `SimulationResult<Result<T, ()>>`, creates nested Results with actor errors
6. **Foundation ergonomics matter**: Small API improvements (format args, choice) greatly improve test readability
7. **Shared state in simulation**: Directory and Storage must be shared across workloads with per-seed clearing
8. **Startup synchronization critical**: Need 1s delay for ServerTransport bind + receive loop before sending messages
9. **Foundation assertions valuable**: Network-level assertions (`peer_queue_*`) show transport layer is active

---

## Phase 2: Network Failure Injection

**Duration**: ~1 hour
**Status**: Not started

### Objectives
- Add chaos to network layer
- Test error handling paths
- Validate timeout mechanisms

### Implementation
- **Increment**: Same test file, add network chaos
- **Buggify Additions**:
  - Random send failures (25% probability)
  - Random network delays (1-100ms)
- **Assertions**:
  - `sometimes_assert!(network_failure_handled, ...)` - Error path triggered
  - `sometimes_assert!(timeout_triggered, ...)` - Request timed out
- **Scale Up**: 50 request-response pairs

### Success Criteria
- [ ] Network failures handled gracefully
- [ ] Timeouts work correctly
- [ ] Invariant: All requests complete (success OR timeout error)
- [ ] No panics on network errors

---

## Phase 3: Concurrent Request Storm

**Duration**: ~1-2 hours
**Status**: Not started

### Objectives
- Test MessageBus under high concurrency
- Find race conditions in CallbackManager
- Validate correlation ID management

### Implementation
- **Increment**: Add massive concurrency
- **Workload**: 100 concurrent requests spawned simultaneously
- **Buggify Additions**:
  - Random routing delays in MessageBus
  - Random callback completion timing
- **Assertions**:
  - `sometimes_assert!(concurrent_requests, count > 10, ...)` - High concurrency
  - `sometimes_assert!(high_pending_count, pending > 10, ...)` - Many pending
- **Invariant**: `pending_count == 0` at test end (no correlation ID leaks)

### Success Criteria
- [ ] 100 concurrent requests complete successfully
- [ ] No correlation ID leaks
- [ ] No deadlocks under concurrent load
- [ ] CallbackManager handles races correctly

---

## Phase 4: Multi-Node Actor Routing

**Duration**: ~2-3 hours
**Status**: Not started

### Objectives
- Test Directory integration with MessageBus
- Test Placement strategy under chaos
- Validate actor location tracking

### Implementation
- **Increment**: Add Directory + Placement layer
- **Setup**: 3 nodes, 20 actors with random placement decisions
- **Buggify Additions**:
  - Stale directory lookups (return old location)
  - Directory lookup failures
  - Placement delays (slow placement decisions)
- **Assertions**:
  - `sometimes_assert!(directory_hit, ...)` - Actor found in directory
  - `sometimes_assert!(directory_miss, ...)` - Actor not registered
  - `sometimes_assert!(remote_forward, ...)` - Message forwarded to remote node
  - `sometimes_assert!(local_activation, ...)` - Actor activated locally
- **Invariant**: Each actor registered in ≤1 location (no duplicate activations)

### Success Criteria
- [ ] Stale directory entries handled (forward to new location)
- [ ] Directory lookup failures trigger placement
- [ ] Placement decisions balanced across nodes
- [ ] No duplicate actor activations (directory race handling works)

---

## Phase 5: Autonomous Chaos Workload

**Duration**: ~3-4 hours
**Status**: Not started

### Objectives
- Implement autonomous testing (operation alphabet pattern)
- Test all failure modes simultaneously
- Achieve comprehensive chaos coverage

### Implementation
- **File**: `moonpool/tests/simulation/autonomous_message_bus.rs`
- **Operation Alphabet**:
  ```rust
  enum MessageBusOp {
      SendRequest(ActorId, NodeId),
      SendOneway(ActorId, Message),
      ActivateActor(ActorId),
      CrashNode(NodeId),
      PartitionNetwork(NodeId, NodeId),
      HealPartition(NodeId, NodeId),
  }
  ```
- **Workload**: 1000 random operations
- **Setup**: 5 nodes, 100 actors
- **All Buggify Enabled**: Every failure mode active
- **Invariants** (always_assert):
  - Message conservation: `messages_received <= messages_sent`
  - No correlation ID leaks: `pending_count == 0` at end
  - Directory consistency: Each actor in ≤1 location
  - No resource leaks: All actors deactivated cleanly

### Success Criteria
- [ ] 100% sometimes_assert coverage across 1000+ seeds
- [ ] All invariants hold
- [ ] No deadlocks under maximum chaos
- [ ] 100% success rate (no failing seeds)

---

## Testing Configuration

### nextest.toml Addition
```toml
[[profile.default.overrides]]
filter = 'test(slow_simulation_messagebus)'
slow-timeout = { period = "180s" }
```

### Iteration Strategy
- **Development**: `IterationControl::FixedCount(10)` - fast iteration
- **Debugging**: Single seed with ERROR logging
- **CI**: `UntilAllSometimesReached(1000)` - comprehensive coverage

### Debug Workflow
1. Note failing seed from test output
2. Set up single-seed replay:
   ```rust
   SimulationBuilder::new()
       .set_seed(failing_seed)
       .set_iteration_control(IterationControl::FixedCount(1))
   ```
3. Enable ERROR-level logging
4. Fix root cause
5. Re-enable chaos testing

---

## Key Principles

1. **Always include I/O**: Every test must have async operations (network, timers)
2. **Start small**: Validate each layer independently
3. **Add complexity gradually**: Network → Routing → Full system
4. **Properties over scenarios**: Express assumptions as assertions
5. **Massive concurrency**: Find races through high load

---

## Progress Tracking

- [x] Phase 1: Minimal 2-Node Request-Response ✅
- [ ] Phase 2: Network Failure Injection
- [ ] Phase 3: Concurrent Request Storm
- [ ] Phase 4: Multi-Node Actor Routing
- [ ] Phase 5: Autonomous Chaos Workload

Each phase is independently valuable and can be committed separately.

---

## Phase 1 Summary

**File**: `moonpool/tests/simulation/minimal_message_bus.rs`
**Status**: ✅ Complete and working
**Test**: `slow_simulation_messagebus_minimal_2node`
**Runtime**: <0.02s for 10 iterations
**Success Rate**: 100%

Phase 1 establishes the foundation for simulation testing of the MessageBus. While simplified from the original plan (no actual MessageBus operations yet), it validates that:
- Simulation framework works with moonpool crate structure
- Async I/O operations function correctly
- Invariant checking integrates properly
- Test infrastructure is ready for Phase 2

**Next Step**: Phase 2 will add actual MessageBus operations with network chaos.
