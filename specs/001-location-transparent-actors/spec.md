# Feature Specification: Location-Transparent Distributed Actor System

**Feature Branch**: `001-location-transparent-actors`
**Created**: 2025-10-21
**Status**: Draft
**Input**: User description: "Build a distributed virtual actor system where developers get actor references by ID without knowing which node hosts them—the system automatically activates actors on-demand, routes messages across nodes via a distributed directory, processes messages one-at-a-time with single-threaded guarantees per actor, and handles node failures with automatic recovery. The core experience should match Orleans' location transparency: define actors using framework conventions, implement async methods that return results, and call them like normal async functions while the framework handles activation/deactivation hooks, directory registration, message routing, request-response correlation, and timeout enforcement. The system must be testable under hostile conditions across multi-node topologies. Exclude advanced features (persistence, transactions, migration, reentrancy, streams) to focus on the core distributed pattern: get reference → transparent routing → remote activation → message delivery → eventual cleanup. Success means a BankAccount actor can deposit/withdraw/check balance across node boundaries reliably with network partitions, node crashes, and directory races all handled correctly, demonstrating location-transparent distributed actors work before adding stateful complexity."

## Clarifications

### Session 2025-10-21

- Q: Should automatic failure recovery (node crash detection, actor reactivation on surviving nodes) be included in this specification? → A: No - remove automatic failure recovery. This requires implementing a full membership protocol which should be a separate specification. Focus on the core distributed actor pattern with a static cluster topology.
- Q: What are the message ordering semantics for messages sent to the same actor? → A: Serial execution without ordering - Messages are processed one-at-a-time but arrival order may differ from send order. No FIFO or total ordering guarantees.
- Q: What consistency model should the directory use? → A: Eventual consistency as default (Orleans model), with directory design allowing future extension to strong consistency for critical virtual actors.
- Q: How are concurrent activation races resolved when multiple nodes try to activate the same actor? → A: Directory is a simple shared structure between machines. Placement algorithm is provided by the directory. Initial algorithm: randomly pick two nodes, then select the one with fewer active actors.
- Q: What are the bounds on actor message queues? → A: Unbounded with monitoring - Queues grow without limit but system provides hooks to detect queue growth for testing and debugging.
- Q: What are the message size limits? → A: Hard size limit with configurable threshold - Messages exceeding the configured maximum size are rejected (configuration knob for the messaging layer).
- Q: Can developers provide custom serialization methods for the message bus? → A: Yes - users can include their own serialization methods for the message bus.
- Q: When an actor throws an exception during message processing, should the actor remain active for subsequent messages or be deactivated? → A: Actor remains active - Exception only fails the single message, actor continues processing subsequent messages (Orleans model).

## User Scenarios & Testing

### User Story 1 - Basic Actor Interaction (Priority: P1)

A developer wants to interact with an actor (e.g., BankAccount) by obtaining a reference using just the actor's ID, without knowing or caring which node in the cluster hosts that actor. The system should transparently locate or activate the actor and deliver messages to it.

**Why this priority**: This is the foundational capability that defines location transparency. Without this working, no other feature has value. It represents the core promise: "get an actor reference by ID and call methods on it as if it's local."

**Independent Test**: Can be fully tested by creating a multi-node cluster, obtaining an actor reference on node A, calling a method, and verifying the actor (possibly on node B) receives and processes the message. Delivers immediate value by demonstrating cross-node actor interaction.

**Acceptance Scenarios**:

1. **Given** a 2-node cluster, **When** node A requests actor reference for "alice" (BankAccount), **Then** the system returns a valid reference regardless of which node will host the actor
2. **Given** an actor reference for "alice", **When** calling `deposit(100)` from any node, **Then** the message is routed to the correct node, the actor is activated if needed, and the operation completes successfully
3. **Given** actor "alice" is hosted on node B, **When** node A calls `check_balance()`, **Then** the request is routed to node B and returns the correct balance
4. **Given** actor "alice" has never been activated, **When** first message arrives, **Then** the system automatically activates the actor on a selected node before processing the message

---

### User Story 2 - Consistent Single-Threaded Actor Execution (Priority: P1)

When multiple messages are sent to the same actor from different nodes, the system must guarantee that messages are processed one-at-a-time in order, maintaining the single-threaded execution model that makes actor programming safe and predictable.

**Why this priority**: This is critical for correctness. Without ordered, single-threaded execution, actor state becomes unpredictable and the entire actor model breaks down. This must work before any other features.

**Independent Test**: Can be fully tested by sending concurrent messages to the same actor from multiple nodes and verifying they are processed sequentially without race conditions. Delivers the safety guarantee that makes actors useful.

**Acceptance Scenarios**:

1. **Given** actor "alice" with balance 0, **When** three deposit messages (100, 200, 300) arrive concurrently from different nodes, **Then** all three are processed sequentially and final balance is 600
2. **Given** actor "alice" is processing a deposit, **When** a withdrawal message arrives, **Then** the withdrawal waits until the deposit completes before executing
3. **Given** actor "alice" receives 100 concurrent messages, **When** all messages complete, **Then** the actor's internal state is consistent (no race conditions or corrupted data)
4. **Given** actor "alice" throws an exception while processing a message, **When** a subsequent message arrives, **Then** the actor remains active and processes the new message normally

---

### User Story 3 - Directory-Based Actor Location (Priority: P2)

The system maintains a distributed directory that tracks which node hosts each activated actor, distributing actors evenly across nodes. Developers never interact with the directory directly—it's an internal system component.

**Why this priority**: The directory is the mechanism that enables location transparency, but it's an implementation detail. Core actor interaction (P1) is more important from a developer perspective.

**Independent Test**: Can be fully tested by activating multiple actors across a cluster, querying the directory (in tests) to verify correct location tracking, and verifying messages are routed to the correct nodes. Delivers the infrastructure for efficient routing.

**Acceptance Scenarios**:

1. **Given** 10 different actors activated across a 3-node cluster, **When** examining directory state, **Then** actors are distributed evenly across the nodes
2. **Given** actor "alice" activates on node B, **When** node A sends a message to "alice", **Then** the directory correctly routes the message to node B
3. **Given** actor "alice" is not yet activated, **When** a message arrives, **Then** the directory assigns a hosting node before activation

---

### User Story 4 - Request-Response with Timeouts (Priority: P3)

When a caller sends a message to an actor and expects a response, the system correlates the response with the original request and enforces configurable timeouts, returning an error if the response doesn't arrive in time.

**Why this priority**: This provides a better developer experience with timeout guarantees, but basic message delivery (P1-P2) must work first. This is a quality-of-life improvement.

**Independent Test**: Can be fully tested by sending a request to an actor, verifying the response is correctly correlated and returned to the caller, and testing timeout enforcement by simulating slow responses. Delivers predictable behavior under load or failures.

**Acceptance Scenarios**:

1. **Given** node A sends request "check_balance" to actor "alice" on node B, **When** actor responds, **Then** the response is correlated and delivered back to the original caller on node A
2. **Given** a request is sent with a 5-second timeout, **When** the actor doesn't respond within 5 seconds, **Then** the caller receives a timeout error
3. **Given** multiple concurrent requests to the same actor from different callers, **When** responses arrive, **Then** each response is correctly matched to its original caller

---

### User Story 5 - Actor Lifecycle Hooks (Priority: P3)

Developers can define activation and deactivation hooks that run when an actor is created or destroyed, allowing initialization and cleanup logic without manual lifecycle management.

**Why this priority**: Lifecycle hooks are important for resource management but are less critical than core message delivery. They enhance the developer experience but aren't required for basic functionality.

**Independent Test**: Can be fully tested by defining an actor with activation/deactivation hooks, triggering activation through messaging, and verifying hooks execute at the correct times. Delivers developer convenience for resource management.

**Acceptance Scenarios**:

1. **Given** an actor defines an activation hook, **When** the first message arrives, **Then** the activation hook executes before any message processing
2. **Given** an actor defines a deactivation hook, **When** the actor is cleaned up due to inactivity, **Then** the deactivation hook executes before the actor is removed
3. **Given** an actor fails during activation hook execution, **When** activation fails, **Then** the system reports an error and the actor is not marked as active

---

### Edge Cases

- How does the system handle messages sent to an actor that is in the middle of being deactivated?
- How does the system handle multiple actors that map to the same node?
- What happens when a response arrives after the request has already timed out?

## Requirements

### Functional Requirements

- **FR-001**: System MUST allow developers to obtain actor references by ID without specifying node location
- **FR-002**: System MUST automatically activate actors on-demand when the first message arrives
- **FR-003**: System MUST route messages to the correct node based on actor location tracked in the distributed directory
- **FR-004**: System MUST process messages for each actor one-at-a-time, maintaining single-threaded execution guarantees (serial execution without message ordering guarantees)
- **FR-005**: System MUST distribute actor placement across available nodes using directory-provided algorithm (initial: two-random-choices with load balancing)
- **FR-006**: System MUST support request-response message patterns with correlation between requests and responses
- **FR-007**: System MUST enforce configurable timeouts on request-response interactions
- **FR-008**: System MUST support developer-defined activation hooks that execute when actors are created
- **FR-009**: System MUST support developer-defined deactivation hooks that execute when actors are cleaned up
- **FR-010**: System MUST handle concurrent activation attempts for the same actor without creating duplicates (directory handles placement decisions atomically)
- **FR-011**: Directory MUST provide eventual consistency guarantees, with architecture allowing future extension to strong consistency for critical actors
- **FR-012**: System MUST support clusters of varying sizes (1 node, 2 nodes, 10+ nodes)
- **FR-013**: System MUST clean up inactive actors after a period of inactivity to prevent unbounded memory growth
- **FR-014**: System MUST return errors to callers when actor message processing fails
- **FR-015**: Actor message queues MUST be unbounded with monitoring hooks provided for queue depth visibility
- **FR-016**: System MUST enforce configurable maximum message size limits and reject messages exceeding the threshold
- **FR-017**: System MUST allow developers to provide custom serialization methods for message bus operations
- **FR-018**: When an actor throws an exception during message processing, the system MUST propagate the error to the caller and keep the actor active for subsequent messages

### Key Entities

- **Actor**: A computational entity identified by a unique ID and type, hosting state and behavior, processing messages one-at-a-time with single-threaded guarantees
- **Actor Reference**: A handle to an actor that allows sending messages without knowing the actor's physical location
- **Message**: A unit of communication sent to an actor, containing either a one-way command or a request expecting a response, with pluggable serialization support for custom formats
- **Directory**: A shared structure across nodes that maps actor IDs to node locations and provides placement algorithms (initial: two-random-choices with load balancing), with eventual consistency by default and architecture supporting future strong consistency extensions
- **Node**: A participant in the cluster that can host actors and route messages
- **Cluster**: The collection of all nodes that work together to provide the actor system

## Success Criteria

### Measurable Outcomes

- **SC-001**: Developers can obtain an actor reference and invoke methods using simple synchronous-looking code (reference retrieval completes in under 100ms in typical cases)
- **SC-002**: Actor messages are processed sequentially without race conditions (100% consistency in concurrent banking operations: deposits, withdrawals, balance checks)
- **SC-003**: System successfully routes messages across node boundaries (100% message delivery in static cluster conditions)
- **SC-004**: System handles hostile testing conditions including concurrent activation races and directory conflicts without deadlocks or data corruption (100 test iterations with 100% success rate)
- **SC-005**: System correctly distributes actors across nodes using two-random-choices algorithm (balanced distribution within 20% variance for 100+ actors)
- **SC-006**: Request-response correlation works correctly (100% response match rate under concurrent load with 1000+ requests)
- **SC-007**: Timeout enforcement is reliable (timeouts trigger within 10% of configured duration)
- **SC-008**: Directory achieves eventual consistency (all nodes converge to consistent actor location view within measurable time window)
- **SC-009**: Activation and deactivation hooks execute reliably (100% hook execution rate during lifecycle transitions)
- **SC-010**: Actor exceptions are isolated to individual messages (actors remain active and process subsequent messages after throwing exceptions)

## Assumptions

- Actors are stateless within their in-memory lifecycle (no persistence between activations)
- Network communication is reliable within the static cluster (no network partitions or node failures)
- Cluster topology is static (fixed set of nodes that do not join or leave during operation)
- Nodes have roughly similar computational capabilities (no specialized hardware requirements)
- Actor IDs are unique strings or numeric identifiers provided by developers
- Default message timeout is 30 seconds unless otherwise specified
- Default maximum message size is 1MB unless otherwise configured
- Inactive actors are cleaned up after 10 minutes of no message activity
- Directory uses in-memory data structures (no persistence of directory state)
- System does not support actor mobility (moving running actors between nodes) in this phase
- Developers are responsible for ensuring actor state fits in memory (no automatic partitioning)

## Out of Scope

The following features are explicitly excluded from this specification:

- **Automatic Failure Recovery**: No node crash detection, actor reactivation on surviving nodes, or membership protocol (will be a separate specification)
- **Dynamic Cluster Membership**: No support for nodes joining or leaving the cluster during operation
- **Network Partition Handling**: No split-brain resolution or partition tolerance mechanisms
- **Persistence**: Actors do not save state to disk or databases
- **Transactions**: No multi-actor transactional operations or two-phase commit
- **Actor Migration**: No live migration of running actors between nodes
- **Reentrancy**: Actors cannot process multiple messages concurrently
- **Streams**: No streaming message delivery or backpressure mechanisms
- **State Partitioning**: Large actor state must fit in single-node memory
- **External Integrations**: No built-in database, message queue, or external service connectors
- **Multi-Tenancy**: No isolation between different actor namespaces or tenants
- **Security**: No authentication, authorization, or encryption (assumed trusted cluster)
- **Monitoring**: No built-in metrics, tracing, or observability infrastructure
