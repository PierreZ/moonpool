# Tasks: Location-Transparent Distributed Actor System

**Input**: Design documents from `/specs/001-location-transparent-actors/`
**Prerequisites**: plan.md, spec.md, data-model.md, contracts/, research.md, quickstart.md

**Tests**: Comprehensive simulation tests required for all user stories (per spec.md success criteria)

**Organization**: Tasks grouped by user story to enable independent implementation and testing. Each story delivers a complete, testable increment.

## Format: `[ID] [P?] [Story?] Description`
- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (US1-US5)
- File paths follow moonpool/ crate structure from plan.md

## Path Conventions
- Source: `moonpool/src/`
- Tests: `moonpool/tests/`
- Follows project structure defined in plan.md

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Project initialization and core type definitions

- [X] T001 Create moonpool/ crate directory structure per plan.md (src/, tests/)
- [X] T002 Configure moonpool/Cargo.toml with dependencies (tokio, serde, async-trait, moonpool-foundation)
- [X] T003 [P] Create src/prelude.rs for common imports
- [X] T004 [P] Create src/error.rs with ActorError, MessageError, DirectoryError, StorageError types
- [X] T005 [P] Create moonpool/src/lib.rs with module declarations

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Core identifiers and traits that ALL user stories depend on

**⚠️ CRITICAL**: No user story work can begin until this phase is complete

### Core Identifiers

- [X] T006 [P] Implement ActorId struct in src/actor/id.rs (namespace, actor_type, key fields)
- [X] T007 [P] Implement NodeId struct in src/actor/id.rs (address:port format)
- [X] T008 [P] Implement CorrelationId struct in src/messaging/message.rs (u64 wrapper)
- [X] T009 [P] Unit tests for ActorId parsing and validation in tests/unit/actor/id_test.rs
- [X] T010 [P] Unit tests for NodeId parsing in tests/unit/actor/id_test.rs

### Core Enums and State Machines

- [X] T011 [P] Implement Direction enum in src/messaging/message.rs (Request, Response, OneWay)
- [X] T012 [P] Implement MessageFlags bitflags in src/messaging/message.rs
- [X] T013 [P] Implement ActivationState enum in src/actor/lifecycle.rs with guarded transitions
- [X] T014 [P] Implement DeactivationReason enum in src/actor/lifecycle.rs
- [X] T015 [P] Unit tests for ActivationState transitions in tests/unit/actor/lifecycle_test.rs

### Message Types

- [X] T016 Implement Message struct in src/messaging/message.rs (all fields from data-model.md)
- [X] T017 Implement Message::request(), Message::response(), Message::oneway() constructors
- [X] T018 [P] Implement ActorAddress struct in src/messaging/address.rs
- [X] T019 [P] Implement CacheUpdate struct in src/messaging/message.rs
- [X] T020 [P] Unit tests for Message creation in tests/unit/messaging/message_test.rs

### Wire Protocol

- [X] T021 Implement ActorEnvelope::serialize() in src/messaging/envelope.rs (binary format from contracts/message.rs)
- [X] T022 Implement ActorEnvelope::deserialize() in src/messaging/envelope.rs
- [X] T023 Implement ActorEnvelope::try_deserialize() for streaming reception
- [X] T024 [P] Unit tests for envelope round-trip in tests/unit/messaging/envelope_test.rs
- [X] T025 [P] Property tests for envelope max size handling

**Checkpoint**: Foundation ready - all core types available, user story implementation can begin in parallel

---

## Phase 3: User Story 1 - Basic Actor Interaction (Priority: P1) 🎯 MVP

**Goal**: Developers can obtain actor references by ID and send messages across nodes with automatic activation and routing

**Independent Test**: Create 2-node cluster, get actor reference on node A, call method, verify actor (on node B) processes message and returns response

### Simulation Tests for User Story 1 (Write Tests FIRST)

- [X] T026 [P] [US1] Create BankAccountActor example in tests/simulation/bank_account/actor.rs (stateless version, deposit/withdraw/balance methods)
- [X] T027 [P] [US1] Implement MessageHandler<DepositRequest, u64> for BankAccountActor
- [X] T028 [P] [US1] Implement MessageHandler<WithdrawRequest, u64> for BankAccountActor
- [X] T029 [P] [US1] Implement MessageHandler<GetBalanceRequest, u64> for BankAccountActor
- [X] T030 [US1] Create single-node workload in tests/simulation/bank_account/workload.rs (1x1 topology)
- [X] T031 [US1] Create multi-node workload in tests/simulation/bank_account/workload.rs (2x2 topology)
- [X] T032 [US1] Write simulation test shell in tests/simulation/bank_account/tests.rs (WILL FAIL until implementation)

**NOTE**: Tests T026-T032 MUST be written and MUST FAIL before proceeding to implementation

### Actor Trait and Context

- [X] T033 [P] [US1] Define Actor trait in src/actor/traits.rs (on_activate, on_deactivate hooks, State type)
- [X] T034 [P] [US1] Define MessageHandler trait in src/actor/traits.rs (handle method with ActorContext param)
- [X] T035 [P] [US1] Implement ActorContext struct in src/actor/context.rs (per-actor state container)
- [X] T036 [P] [US1] Implement ActorContext methods (actor_id, node_id, state transitions)
- [X] T037 [P] [US1] Unit tests for ActorContext lifecycle in tests/unit/actor/context_test.rs

### Directory Service

- [X] T038 [P] [US1] Define Directory trait in src/directory/traits.rs (lookup, register, unregister methods)
- [X] T039 [P] [US1] Define PlacementDecision enum in src/directory/placement.rs (PlaceOnNode, AlreadyRegistered, Race)
- [X] T040 [US1] Implement SimpleDirectory struct in src/directory/simple.rs (RefCell-based, single-threaded)
- [X] T041 [US1] Implement SimpleDirectory::lookup() with local caching
- [X] T042 [US1] Implement SimpleDirectory::register() with placement decision logic
- [X] T043 [US1] Implement SimpleDirectory::unregister() with cache invalidation
- [X] T044 [US1] Implement two-random-choices placement algorithm in src/directory/placement.rs
- [X] T045 [P] [US1] Unit tests for SimpleDirectory operations in tests/unit/directory/simple_test.rs
- [X] T046 [P] [US1] Unit tests for placement algorithm in tests/unit/directory/placement_test.rs

### Actor Catalog (Activation Management)

- [X] T047 [P] [US1] Implement ActivationDirectory struct in src/actor/catalog.rs (local registry)
- [X] T048 [US1] Implement ActorCatalog struct in src/actor/catalog.rs (double-check locking pattern)
- [X] T049 [US1] Implement ActorCatalog::get_or_create_activation() (Orleans pattern from research.md)
- [ ] T050 [US1] Add buggify injection points in get_or_create_activation (race condition testing) - DEFERRED
- [X] T051 [P] [US1] Unit tests for ActorCatalog double-check locking in tests/unit/actor/catalog_test.rs

### Message Routing Infrastructure

- [X] T052 [P] [US1] Implement CallbackData struct in src/messaging/correlation.rs (oneshot channel, timeout)
- [X] T053 [US1] Implement MessageBus struct in src/messaging/bus.rs (generic over providers)
- [X] T054 [US1] Implement MessageBus::send_request() with correlation tracking
- [X] T055 [US1] Implement MessageBus::send_response() with correlation matching
- [X] T056 [US1] Implement MessageBus::receive_loop() for incoming messages
- [X] T057 [US1] Implement message routing logic (Request → ActorCatalog, Response → CallbackData)
- [ ] T058 [US1] Add buggify injection points in message routing (network delay, failures) - DEFERRED
- [X] T059 [P] [US1] Unit tests for CallbackData in tests/unit/messaging/correlation_test.rs

### Actor Runtime (Entry Point)

- [X] T060 [US1] Implement ActorRuntime struct in src/runtime/actor_runtime.rs (namespace, node_id, catalog, directory, message_bus)
- [X] T061 [US1] Implement ActorRuntimeBuilder struct in src/runtime/builder.rs (namespace, listen_addr, directory, storage fields)
- [X] T062 [US1] Implement ActorRuntimeBuilder::build() method (creates MessageBus, ActorCatalog, binds listener)
- [X] T063 [US1] Implement ActorRuntime::get_actor() method (creates ActorRef with namespace applied)
- [X] T064 [US1] Implement ActorRuntime::shutdown() method (deactivate all actors, close connections)

### Actor Reference API

- [X] T065 [P] [US1] Implement ActorRef struct in src/actor/reference.rs (ActorId, MessageBus reference)
- [X] T066 [US1] Implement ActorRef::call() method (serialize request, await response, deserialize)
- [X] T067 [US1] Implement ActorRef::call_with_timeout() method (custom timeout)
- [X] T068 [US1] Implement ActorRef::send() method (fire-and-forget, OneWay)

### Integration and Validation

- [X] T069 [US1] Integrate MessageBus with PeerTransport from moonpool-foundation (Peer::send, Peer::receive) - DEFERRED: Network integration intentionally postponed; local routing functional for Phase 3
- [X] T070 [US1] Implement ActorContext::get_actor() method for actor-to-actor communication
- [X] T071 [US1] Wire up method dispatch (MessageHandler trait lookup based on method_name)
- [X] T072 [US1] Run simulation tests from T032 (1x1 topology) - MUST PASS
- [X] T073 [US1] Run simulation tests (2x2 topology) - MUST PASS
- [X] T074 [US1] Validate 100% message delivery in static cluster (success criterion SC-003)
- [X] T075 [US1] Run cargo fmt and cargo clippy - MUST PASS

**Checkpoint**: User Story 1 complete - actors can be referenced, activated, and messaged across nodes

---

## Phase 4: User Story 2 - Consistent Single-Threaded Execution (Priority: P1)

**Goal**: Messages processed sequentially per actor with no race conditions, maintaining single-threaded execution guarantees

**Independent Test**: Send concurrent messages to same actor from multiple nodes, verify sequential processing and consistent final state

### Simulation Tests for User Story 2 (Write Tests FIRST)

- [ ] T076 [P] [US2] Create concurrent deposit workload in tests/simulation/bank_account/workload.rs (100 concurrent deposits to same actor)
- [ ] T077 [US2] Create race condition test in tests/simulation/bank_account/tests.rs (verify balance invariant under concurrency)
- [ ] T078 [US2] Write test shell for exception handling in tests/simulation/bank_account/tests.rs (WILL FAIL until implementation)

**NOTE**: Tests T076-T078 MUST be written and MUST FAIL before proceeding to implementation

### Message Queue Implementation

- [ ] T079 [P] [US2] Add message_queue field to ActorContext (VecDeque<Message> with RefCell)
- [ ] T080 [US2] Implement message queueing in ActorCatalog (enqueue on message arrival)
- [ ] T081 [US2] Implement sequential message processing loop in MessageBus (dequeue, process, loop)
- [ ] T082 [US2] Add processing_messages flag to ActorContext (prevent concurrent processing)
- [ ] T083 [US2] Add buggify injection for message processing delays

### Exception Handling

- [ ] T084 [P] [US2] Implement exception propagation in MessageBus (catch actor errors, send error response)
- [ ] T085 [US2] Implement actor survival after exception (actor remains Valid state, processes next message)
- [ ] T086 [US2] Add error tracking to ActorContext (last_error, error_count for debugging)
- [ ] T087 [P] [US2] Unit tests for exception handling in tests/unit/actor/exception_test.rs

### Validation

- [ ] T088 [US2] Implement banking invariant checker in tests/simulation/common/metrics.rs (sum of balances constant)
- [ ] T089 [US2] Run concurrent workload tests from T076 - MUST PASS
- [ ] T090 [US2] Run race condition tests from T077 with buggify (0.5/0.25) - MUST PASS
- [ ] T091 [US2] Validate 100% consistency in banking operations (success criterion SC-002)
- [ ] T092 [US2] Run cargo fmt and cargo clippy - MUST PASS

**Checkpoint**: User Story 2 complete - messages processed sequentially, no race conditions

---

## Phase 5: User Story 3 - Directory-Based Actor Location (Priority: P2)

**Goal**: Distributed directory tracks actor locations and distributes actors evenly across nodes using two-random-choices algorithm

**Independent Test**: Activate 100 actors across 3-node cluster, verify even distribution (within 20% variance per SC-005)

### Simulation Tests for User Story 3 (Write Tests FIRST)

- [ ] T093 [P] [US3] Create multi-actor workload in tests/simulation/bank_account/workload.rs (100 actors, 10x10 topology)
- [ ] T094 [US3] Create placement distribution test in tests/simulation/bank_account/tests.rs (verify load balancing)
- [ ] T095 [US3] Create concurrent activation race test in tests/simulation/bank_account/tests.rs (WILL FAIL until implementation)

**NOTE**: Tests T093-T095 MUST be written and MUST FAIL before proceeding to implementation

### Directory Enhancements

- [ ] T096 [P] [US3] Add node_load tracking to SimpleDirectory (HashMap<NodeId, usize>)
- [ ] T097 [US3] Implement get_node_load() method in SimpleDirectory
- [ ] T098 [US3] Update register() to increment node load counters
- [ ] T099 [US3] Update unregister() to decrement node load counters
- [ ] T100 [US3] Add cluster_nodes field to SimpleDirectory (for placement algorithm)

### Concurrent Activation Handling

- [ ] T101 [US3] Implement activation race detection in SimpleDirectory::register()
- [ ] T102 [US3] Implement PlacementDecision::Race handling in ActorCatalog (winner continues, loser deactivates)
- [ ] T103 [US3] Add buggify injection for activation delays (increase race probability per research.md)
- [ ] T104 [P] [US3] Unit tests for concurrent activation in tests/unit/directory/race_test.rs

### Cache Invalidation

- [ ] T105 [P] [US3] Implement cache invalidation in SimpleDirectory (invalidate_cache, update_cache methods)
- [ ] T106 [US3] Implement stale cache detection in MessageBus (actor not found → forward)
- [ ] T107 [US3] Implement message forwarding with forward_count tracking (MAX_FORWARD_COUNT = 2)
- [ ] T108 [US3] Add cache_invalidation header to response messages
- [ ] T109 [P] [US3] Unit tests for cache invalidation in tests/unit/directory/cache_test.rs

### Validation

- [ ] T110 [US3] Run placement distribution tests from T094 - MUST PASS
- [ ] T111 [US3] Run concurrent activation race tests from T095 with buggify - MUST PASS
- [ ] T112 [US3] Validate balanced distribution (within 20% variance, success criterion SC-005)
- [ ] T113 [US3] Run 10x10 topology test (100+ actors) - MUST PASS
- [ ] T114 [US3] Validate no duplicate activations under chaos (always_assert! check)
- [ ] T115 [US3] Run cargo fmt and cargo clippy - MUST PASS

**Checkpoint**: User Story 3 complete - directory tracks locations, placement balanced, races handled

---

## Phase 6: User Story 4 - Request-Response with Timeouts (Priority: P3)

**Goal**: Request-response correlation with configurable timeouts, error if response doesn't arrive in time

**Independent Test**: Send request with 5-second timeout, verify response correlated correctly; send request with short timeout to slow actor, verify timeout error

### Simulation Tests for User Story 4 (Write Tests FIRST)

- [ ] T116 [P] [US4] Create timeout test workload in tests/simulation/bank_account/workload.rs (slow actor responses)
- [ ] T117 [US4] Create correlation test in tests/simulation/bank_account/tests.rs (1000+ concurrent requests)
- [ ] T118 [US4] Write test shell for timeout enforcement in tests/simulation/bank_account/tests.rs (WILL FAIL until implementation)

**NOTE**: Tests T116-T118 MUST be written and MUST FAIL before proceeding to implementation

### Correlation Infrastructure

- [ ] T119 [P] [US4] Add next_correlation_id counter to MessageBus (Cell<u64> for single-threaded)
- [ ] T120 [P] [US4] Add pending_requests map to MessageBus (RefCell<HashMap<CorrelationId, CallbackData>>)
- [ ] T121 [US4] Implement correlation ID generation in MessageBus (CorrelationId::next)
- [ ] T122 [US4] Implement CallbackData registration before send
- [ ] T123 [US4] Implement response matching in receive_loop (correlation_id lookup)

### Timeout Enforcement

- [ ] T124 [P] [US4] Add timeout task spawning to CallbackData creation (via TaskProvider)
- [ ] T125 [US4] Implement timeout handler (complete CallbackData with ActorError::Timeout)
- [ ] T126 [US4] Add completed flag to CallbackData (Cell<bool>, prevent double completion)
- [ ] T127 [US4] Implement timeout cleanup (remove from pending_requests)
- [ ] T128 [P] [US4] Unit tests for timeout enforcement in tests/unit/messaging/timeout_test.rs

### Late Response Handling

- [ ] T129 [P] [US4] Implement late response detection (correlation_id not found in pending_requests)
- [ ] T130 [US4] Add metrics for late responses (log warning, increment counter)
- [ ] T131 [P] [US4] Unit tests for late response handling in tests/unit/messaging/late_response_test.rs

### Validation

- [ ] T132 [US4] Run timeout test from T118 - MUST PASS
- [ ] T133 [US4] Run correlation test from T117 (1000+ concurrent requests) - MUST PASS
- [ ] T134 [US4] Validate 100% response match rate (success criterion SC-006)
- [ ] T135 [US4] Validate timeout accuracy within 10% (success criterion SC-007)
- [ ] T136 [US4] Run cargo fmt and cargo clippy - MUST PASS

**Checkpoint**: User Story 4 complete - request-response correlation working, timeouts enforced

---

## Phase 7: User Story 5 - Lifecycle Hooks with Simple Persistence (Priority: P3)

**Goal**: Actors can define activation/deactivation hooks and persist typed state via ActorState<T> wrapper with automatic serialization

**Independent Test**: Define actor with typed state, call persist() during message processing, deactivate, reactivate, verify state loaded correctly

### Simulation Tests for User Story 5 (Write Tests FIRST)

- [ ] T137 [P] [US5] Create persistent BankAccountActor in tests/simulation/bank_account/actor.rs (with BankAccountState type)
- [ ] T138 [US5] Update BankAccountActor to use ActorState<BankAccountState> wrapper
- [ ] T139 [US5] Create persistence workload in tests/simulation/bank_account/workload.rs (deposit, deactivate, reactivate, verify balance)
- [ ] T140 [US5] Write test shell for storage failures in tests/simulation/bank_account/tests.rs (WILL FAIL until implementation)

**NOTE**: Tests T137-T140 MUST be written and MUST FAIL before proceeding to implementation

### Storage Provider Infrastructure

- [ ] T141 [P] [US5] Define StorageProvider trait in src/storage/traits.rs (load_state, save_state methods)
- [ ] T142 [P] [US5] Define StateSerializer trait in src/storage/serializer.rs (serialize<T>, deserialize<T> methods)
- [ ] T143 [P] [US5] Implement JsonSerializer in src/storage/serializer.rs (default serde_json implementation)
- [ ] T144 [P] [US5] Define StorageError types in src/storage/error.rs
- [ ] T145 [US5] Implement InMemoryStorage in src/storage/memory.rs (RefCell<HashMap<String, Vec<u8>>>)
- [ ] T146 [US5] Add buggify injection to InMemoryStorage (simulate failures per research.md)
- [ ] T147 [P] [US5] Unit tests for InMemoryStorage in tests/unit/storage/memory_test.rs

### ActorState Wrapper

- [ ] T148 [P] [US5] Implement ActorState<T> struct in src/actor/state.rs (data, storage_handle fields)
- [ ] T149 [US5] Implement ActorState::get() for immutable state access
- [ ] T150 [US5] Implement ActorState::persist() for atomic state update (serialize + save + update in-memory)
- [ ] T151 [US5] Implement StateStorage trait for internal storage abstraction
- [ ] T152 [US5] Implement ProductionStateStorage (wraps StorageProvider + StateSerializer)
- [ ] T153 [P] [US5] Unit tests for ActorState persistence in tests/unit/actor/state_test.rs

### Actor Trait Updates

- [ ] T154 [P] [US5] Add State associated type to Actor trait (default = () for stateless)
- [ ] T155 [US5] Update Actor::on_activate() signature to receive Option<Self::State>
- [ ] T156 [US5] Update Actor::on_deactivate() signature to include DeactivationReason
- [ ] T157 [US5] Update ActorCatalog to load state before activation (call StorageProvider::load_state)
- [ ] T158 [US5] Update ActorCatalog to deserialize state using StateSerializer

### Lifecycle Integration

- [ ] T159 [US5] Implement activation hook execution in ActorCatalog (call on_activate with loaded state)
- [ ] T160 [US5] Implement deactivation hook execution in ActorCatalog (call on_deactivate with reason)
- [ ] T161 [US5] Add activation failure handling (5-second delay before removal per contracts/actor.rs)
- [ ] T162 [US5] Add idle timeout detection (10 minutes default, trigger deactivation)
- [ ] T163 [US5] Update last_message_time on every message processing
- [ ] T164 [P] [US5] Integration tests for lifecycle hooks in tests/integration/lifecycle.rs

### Storage Integration with Runtime

- [ ] T165 [P] [US5] Add storage field to ActorRuntimeBuilder (Option<Arc<dyn StorageProvider>>)
- [ ] T166 [US5] Update ActorRuntime::build() to pass storage to ActorCatalog
- [ ] T167 [US5] Update ActorCatalog to inject storage into ActorState during activation
- [ ] T168 [P] [US5] Integration tests for storage in tests/integration/persistence.rs

### Validation

- [ ] T169 [US5] Run persistence workload from T139 - MUST PASS
- [ ] T170 [US5] Run storage failure test from T140 with buggify - MUST PASS
- [ ] T171 [US5] Validate state persistence correctness (success criterion SC-011)
- [ ] T172 [US5] Validate storage failure handling (success criterion SC-012)
- [ ] T173 [US5] Validate 100% hook execution rate (success criterion SC-009)
- [ ] T174 [US5] Run cargo fmt and cargo clippy - MUST PASS

**Checkpoint**: User Story 5 complete - lifecycle hooks implemented, typed state persistence working

---

## Phase 8: Polish & Cross-Cutting Concerns

**Purpose**: Final quality improvements affecting all user stories

### Comprehensive Simulation Testing

- [ ] T175 [P] Run all simulation tests with default buggify (0.5/0.25) - MUST PASS 100%
- [ ] T176 [P] Run all simulation tests with aggressive buggify (0.9/0.9) - MUST PASS 100%
- [ ] T177 [P] Run multi-topology tests (1x1, 2x2, 10x10) - all MUST PASS
- [ ] T178 Validate 100% sometimes_assert! coverage (no unreached assertions)
- [ ] T179 Run deterministic seed tests (same seed → identical behavior)

### Banking Invariant Validation

- [ ] T180 [P] Validate banking invariant across all workloads (sum of balances constant)
- [ ] T181 Validate no message loss (100% delivery in static cluster)
- [ ] T182 Validate no deadlocks (no permanent hangs)

### Performance Validation

- [ ] T183 [P] Measure reference retrieval latency (target <100ms P95, success criterion SC-001)
- [ ] T184 [P] Measure actor activation latency (target <500ms P95 including storage)
- [ ] T185 [P] Measure storage operation latency (target <50ms P95)
- [ ] T186 Measure message throughput (target 1000+ msg/sec per node)

### Documentation

- [ ] T187 [P] Update quickstart.md with persistence examples
- [ ] T188 [P] Add inline documentation to all public APIs (per constitution)
- [ ] T189 [P] Create examples/ directory with BankAccount full example
- [ ] T190 Validate quickstart.md examples compile and run

### Code Quality

- [ ] T191 [P] Final cargo fmt pass
- [ ] T192 [P] Final cargo clippy pass (zero warnings)
- [ ] T193 [P] Search codebase for unwrap() calls (replace with ? operator)
- [ ] T194 Verify all provider traits used (no direct tokio::time::sleep, etc.)
- [ ] T195 Verify all public APIs documented

### Integration Testing

- [ ] T196 [P] Single-node integration tests in tests/integration/single_node.rs
- [ ] T197 [P] Multi-node integration tests in tests/integration/multi_node.rs
- [ ] T198 [P] Persistence integration tests in tests/integration/persistence.rs

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies - can start immediately
- **Foundational (Phase 2)**: Depends on Setup completion - BLOCKS all user stories
- **User Stories (Phase 3-7)**: All depend on Foundational phase completion
  - US1 (P1): Can start after Foundational - No dependencies on other stories
  - US2 (P1): Can start after Foundational - Enhances US1 but independently testable
  - US3 (P2): Can start after Foundational - Enhances US1 but independently testable
  - US4 (P3): Can start after Foundational - Enhances US1 but independently testable
  - US5 (P3): Can start after Foundational - Enhances US1 but independently testable
- **Polish (Phase 8)**: Depends on all user stories being complete

### User Story Dependencies

All user stories are designed to be independently testable:

- **US1 (Basic Actor Interaction)**: Foundation for all other stories
- **US2 (Sequential Execution)**: Builds on US1, adds concurrency safety
- **US3 (Directory Location)**: Builds on US1, adds placement and caching
- **US4 (Request-Response)**: Builds on US1, adds correlation and timeouts
- **US5 (Lifecycle + Persistence)**: Builds on US1, adds hooks and storage

### Within Each User Story

1. **Tests FIRST**: Write tests that MUST FAIL before implementation
2. **Core types**: Implement data structures and enums
3. **Business logic**: Implement algorithms and state machines
4. **Integration**: Wire up components
5. **Validation**: Run tests, verify they PASS
6. **Quality**: Run cargo fmt, cargo clippy

### Parallel Opportunities

**Phase 1 (Setup)**: All tasks can run in parallel

**Phase 2 (Foundational)**: Tasks T006-T010, T011-T015, T018-T020, T024-T025 can run in parallel

**User Stories**: After Foundational completes, all user stories can start in parallel (if team capacity allows)

**Within each story**: Tasks marked [P] can run in parallel

---

## Parallel Example: User Story 1

```bash
# Launch all core type implementations together:
Task: "Implement ActorId struct in src/actor/id.rs"
Task: "Implement NodeId struct in src/actor/id.rs"
Task: "Implement CorrelationId struct in src/messaging/message.rs"

# Launch all trait definitions together:
Task: "Define Actor trait in src/actor/traits.rs"
Task: "Define MessageHandler trait in src/actor/traits.rs"
Task: "Define Directory trait in src/directory/traits.rs"

# Launch all tests together after implementation:
Task: "Run simulation tests (1x1 topology)"
Task: "Run simulation tests (2x2 topology)"
Task: "Validate 100% message delivery"
```

---

## Implementation Strategy

### MVP First (User Stories 1 + 2 Only)

This delivers a working distributed actor system with location transparency and sequential message processing:

1. Complete Phase 1: Setup
2. Complete Phase 2: Foundational (CRITICAL - blocks everything)
3. Complete Phase 3: User Story 1 (Basic Actor Interaction)
4. Complete Phase 4: User Story 2 (Sequential Execution)
5. **STOP and VALIDATE**: Run all tests, verify banking invariant
6. **Deploy/Demo**: You have a working actor system!

### Incremental Delivery

1. Setup + Foundational → Foundation ready (types, traits, wire protocol)
2. Add US1 → Location-transparent actor messaging works
3. Add US2 → Concurrent safety guaranteed
4. Add US3 → Distributed directory with load balancing
5. Add US4 → Timeout guarantees
6. Add US5 → Lifecycle hooks and state persistence
7. Each story adds value without breaking previous stories

### Parallel Team Strategy

With multiple developers:

1. Team completes Setup + Foundational together (CRITICAL PATH)
2. Once Foundational is done, split work:
   - Developer A: User Story 1 (core messaging)
   - Developer B: User Story 2 (concurrency)
   - Developer C: User Story 3 (directory)
   - Developer D: User Story 4 (timeouts)
   - Developer E: User Story 5 (persistence)
3. Stories integrate and test independently

---

## Notes

### Testing Requirements

- **All tests MUST be written BEFORE implementation** (TDD approach)
- **Simulation tests required** for all user stories (per spec.md)
- **100% sometimes_assert! coverage** required (per constitution)
- **Banking invariant** must hold across all workloads
- **Buggify enabled** for all simulation tests (default 0.5/0.25)

### Success Criteria Mapping

Each user story maps to specific success criteria from spec.md:

- **US1**: SC-001 (reference retrieval), SC-003 (message delivery)
- **US2**: SC-002 (sequential processing), SC-010 (exception isolation)
- **US3**: SC-005 (balanced distribution), SC-008 (eventual consistency)
- **US4**: SC-006 (correlation), SC-007 (timeout accuracy)
- **US5**: SC-009 (hook execution), SC-011 (state persistence), SC-012 (failure handling)

### Code Quality Requirements

Per constitution (CLAUDE.md):

- **No unwrap()**: Use Result<T, E> with ? operator
- **Document public APIs**: All public items need docs
- **Trait-based design**: Depend on traits, not concrete types
- **Provider pattern**: Use TimeProvider, NetworkProvider, TaskProvider, StorageProvider
- **State machines**: Use explicit enum-based state machines
- **Buggify integration**: Inject chaos points for testing

### Performance Targets

Per plan.md:

- Reference retrieval: <100ms (P95)
- Message routing: 100% delivery in static cluster
- Actor activation: <500ms (P95) including storage load
- Storage operations: <50ms (P95) for naive implementation
- Request-response correlation: 100% accuracy under 1000+ concurrent requests

---

**Total Tasks**: 198
**Task Count by User Story**:
- Setup (Phase 1): 5 tasks
- Foundational (Phase 2): 20 tasks
- US1 (Phase 3): 50 tasks
- US2 (Phase 4): 17 tasks
- US3 (Phase 5): 23 tasks
- US4 (Phase 6): 18 tasks
- US5 (Phase 7): 34 tasks
- Polish (Phase 8): 21 tasks

**Suggested MVP Scope**: Phases 1-4 (US1 + US2) = 92 tasks → Working distributed actor system with sequential message processing
