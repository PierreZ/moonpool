# Discovering Properties

<!-- toc -->

You know the assertion macros. You know the buggify patterns. But when you sit down with a fresh Process and Workload, the hardest question is not **how** to assert — it is **what** to assert. Where should `assert_always!` go? Which code paths need `assert_sometimes!`? Where would `buggify!()` expose the most interesting failures?

A single unstructured pass through your code will find the obvious properties. The non-obvious ones — the properties that catch real bugs — require looking at the same code from multiple independent angles.

## The Attention Focus Pattern

The idea comes from Antithesis's property discovery methodology. Instead of reading through code once and noting assertions as they occur to you, you examine the same code **eight times**, each time through a different lens. Each lens is called an **attention focus**.

Why does this work? Because different failure modes hide in different mental models. A developer thinking about crash recovery notices different things than one thinking about concurrency. A focus on protocol contracts surfaces properties that a focus on resource boundaries would miss entirely. The structured repetition is the point.

## Eight Focuses for Moonpool Code

**State Integrity** examines in-memory invariants and storage persistence. What state must survive reboots? What write ordering assumptions exist? If a process is killed between writing field A and field B, does recovery handle the inconsistency? Look for monotonicity properties (ballot numbers, sequence IDs), derived state that could diverge from its source, and reference model expectations that the process might violate.

**Concurrency** looks at races between workloads hitting the same process. Moonpool is single-threaded, but async interleaving across `.await` points creates real concurrency hazards. Check-then-act patterns where another task could mutate state between the check and the act. Multiple workloads accessing the same key. `Rc<RefCell<>>` state touched across yield points.

**Crash Recovery** asks what happens when a Process is killed and restarted from its factory. The factory returns a blank instance — all in-memory state is gone. Partially-written storage operations (write without `sync_all()`) may leave corrupt data. Recovery code that assumes clean state will miss torn writes. Operations interrupted between a storage write and a sync are the classic source of subtle bugs.

**Network Faults** examines behavior under connection drops, partitions, and reordering. RPC calls without timeouts. Retry logic that is not idempotent. Stale cached state after a partition heals (old leader references, expired peer info). Fire-and-forget sends where delivery actually matters.

**Timing & Scheduling** checks sensitivity to event ordering. Hardcoded timeouts that interact with other timeouts. Logic that assumes timers fire before network events arrive. Election timeouts that could overlap with heartbeat timeouts. Races between timer expiry and data delivery.

**Resource Boundaries** hunts for unbounded growth. `Vec` or `VecDeque` that grows without limit under sustained load. Missing backpressure on incoming requests. Operations that assume connections or file handles are always available. These bugs only surface under chaos, which is exactly when they matter most.

**Protocol Contracts** looks for guarantees that are claimed but not enforced. Doc comments that say "returns error if not found" but the code returns a default. Ordering assumptions between RPC calls (create before update) that nothing validates. Response types that mask partial failures as success.

**Lifecycle Transitions** examines startup, shutdown, and reboot sequences. Requests arriving before initialization completes. In-flight work silently dropped during graceful shutdown. State published to `StateRegistry` before it is valid. Shutdown ordering dependencies where closing connections before flushing storage loses data.

## Using the Focuses

There are two ways to work through the focuses.

**Ensemble mode** runs all focuses in parallel. For each focus, a separate analysis pass examines the code through that single lens and produces candidate assertions and buggify points. The results are then synthesized: duplicates found by multiple focuses are high-confidence properties, while unique finds from a single focus are high-value catches that a single pass would have missed.

**Sequential mode** works through the focuses as a checklist. The key discipline is making an explicit pass for each focus. Do not skip a focus because an earlier pass "already covered" that area. The value is in the independent perspective — the same line of code looks different through the lens of crash recovery than through the lens of concurrency.

## What Discovery Produces

Each discovered property maps to a concrete assertion or buggify placement:

- **Location**: exact file and line range
- **Macro**: which assertion type (`assert_always!`, `assert_sometimes!`, `buggify!()`, etc.)
- **Message**: unique, descriptive assertion message
- **Rationale**: what bug this catches and why it matters
- **Provenance**: which focus or focuses surfaced it

Properties found by multiple focuses independently are strong candidates. They represent invariants that sit at the intersection of multiple failure modes. Properties found by only one focus are equally important — they represent the blind spots that unstructured review misses.

## A Quick Example

Consider a key-value Process that accepts writes over RPC and persists them to storage. A single review pass might add `assert_always!` for "response matches stored value" and call it done.

Running through the focuses reveals more:

- **State Integrity**: the process maintains a write counter. Is it monotonically increasing? `assert_always!(new_count > old_count, "write counter regression")`
- **Crash Recovery**: writes go to an in-memory map, then flush to storage on a timer. A crash between write and flush loses data. `buggify!()` on the flush path to test this. `assert_sometimes!` to verify the recovery path is actually exercised.
- **Concurrency**: two workloads writing the same key. The last-write-wins semantics should hold. `assert_always!` on read-after-write consistency from each workload's perspective.
- **Network Faults**: the client retries on timeout, but the write is not idempotent. A retry after an ambiguous failure could double-apply. `assert_always!` on the write count matching expected count.

Four focuses, four distinct properties, one of which (the retry idempotency bug) is the kind of subtle issue that crashes production systems. A single pass would likely have caught only the first.

The `/discover-properties` skill automates this process. It examines your Process and Workload code through all eight focuses and produces a structured list of assertion and buggify placements, ready to implement.
