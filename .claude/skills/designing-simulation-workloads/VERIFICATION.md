# Verification Patterns Deep Dive

This file provides comprehensive guidance on the three main verification patterns for autonomous workloads.

## Overview

Verification answers the question: "How do I know my system is correct after running 1000 random operations?"

Three approaches:
1. **Reference Implementation**: Mirror with simple, correct code
2. **Operation Logging**: Record and replay operations
3. **Invariant Tracking**: Maintain mathematical properties

## Pattern 1: Reference Implementation

### When to Use

- Complex production logic with simple correctness rules
- Clear specification (e.g., "behaves like a map")
- Small state space (reference fits in memory)

### Examples

#### Key-Value Store

```rust
struct ReferenceKV {
    // Simple HashMap as "truth"
    reference: HashMap<String, Vec<u8>>,
    // Complex distributed system under test
    distributed: DistributedKV,
}

async fn verify_kv_operation(
    reference: &mut HashMap<String, Vec<u8>>,
    distributed: &DistributedKV,
    op: KVOp,
) -> Result<()> {
    match op {
        KVOp::Put(key, value) => {
            // Apply to both
            reference.insert(key.clone(), value.clone());
            distributed.put(key, value).await?;

            sometimes_assert!(
                put_operation,
                true,
                "Put operation executed"
            );
        }
        KVOp::Get(key) => {
            let expected = reference.get(&key).cloned();
            let actual = distributed.get(&key).await?;

            always_assert!(
                get_matches,
                actual == expected,
                format!("Get mismatch: key={:?}, expected={:?}, actual={:?}",
                        key, expected, actual)
            );

            sometimes_assert!(
                get_operation,
                true,
                "Get operation executed"
            );
        }
        KVOp::Delete(key) => {
            reference.remove(&key);
            distributed.delete(&key).await?;

            sometimes_assert!(
                delete_operation,
                true,
                "Delete operation executed"
            );
        }
        KVOp::List => {
            let expected: HashSet<_> = reference.keys().cloned().collect();
            let actual: HashSet<_> = distributed.list_keys().await?
                .into_iter()
                .collect();

            always_assert!(
                list_matches,
                actual == expected,
                format!("List mismatch: expected {} keys, got {}",
                        expected.len(), actual.len())
            );

            sometimes_assert!(
                list_operation,
                true,
                "List operation executed"
            );
        }
    }

    Ok(())
}
```

#### Actor Directory

```rust
struct ReferenceDirectory {
    // Simple HashMap: ActorId -> NodeId
    reference: HashMap<ActorId, NodeId>,
    // Complex distributed directory
    distributed: DistributedDirectory,
}

async fn verify_directory_operation(
    reference: &mut HashMap<ActorId, NodeId>,
    distributed: &DistributedDirectory,
    op: DirectoryOp,
) -> Result<()> {
    match op {
        DirectoryOp::Register(actor_id, node_id) => {
            reference.insert(actor_id.clone(), node_id);
            distributed.register(&actor_id, node_id).await?;

            sometimes_assert!(
                register_operation,
                true,
                "Register operation executed"
            );
        }
        DirectoryOp::Lookup(actor_id) => {
            let expected = reference.get(&actor_id).copied();
            let actual = distributed.lookup(&actor_id).await?;

            always_assert!(
                lookup_matches,
                actual == expected,
                format!("Lookup mismatch: actor={:?}, expected={:?}, actual={:?}",
                        actor_id, expected, actual)
            );

            sometimes_assert!(
                lookup_operation,
                true,
                "Lookup operation executed"
            );
        }
        DirectoryOp::Unregister(actor_id) => {
            reference.remove(&actor_id);
            distributed.unregister(&actor_id).await?;

            sometimes_assert!(
                unregister_operation,
                true,
                "Unregister operation executed"
            );
        }
    }

    Ok(())
}
```

### Advantages

- **Clear correctness**: Reference is obviously correct
- **Immediate feedback**: Detect mismatches during execution
- **Simple to implement**: Just mirror operations

### Disadvantages

- **Memory overhead**: Reference state grows with system
- **Not always applicable**: Some systems don't have simple reference
- **Timing differences**: Reference is synchronous, system is async

---

## Pattern 2: Operation Logging

### When to Use

- Complex state that's hard to mirror
- Deterministic replay is possible
- Want to analyze operation sequences that cause failures

### Examples

#### Message Bus Ordering

```rust
#[derive(Clone, Debug)]
struct LoggedOp {
    timestamp: Instant,
    operation: MessageBusOp,
}

struct MessageBusVerifier {
    log: Vec<LoggedOp>,
    system: MessageBus,
}

async fn execute_and_log(
    log: &mut Vec<LoggedOp>,
    system: &MessageBus,
    op: MessageBusOp,
) -> Result<()> {
    let logged_op = LoggedOp {
        timestamp: Instant::now(),
        operation: op.clone(),
    };

    log.push(logged_op);

    match op {
        MessageBusOp::SendMessage(from, to, msg) => {
            system.send_message(from, to, msg).await?;

            sometimes_assert!(
                message_sent,
                true,
                "Message sent"
            );
        }
        // ... other operations
    }

    Ok(())
}

async fn verify_log_consistency(
    log: &[LoggedOp],
    system: &MessageBus,
) -> Result<()> {
    // Analyze operation log for violations

    // Example: Check FIFO ordering per pair
    let mut per_pair_messages: HashMap<(NodeId, NodeId), Vec<MessageId>> = HashMap::new();

    for logged_op in log {
        if let MessageBusOp::SendMessage(from, to, msg) = &logged_op.operation {
            per_pair_messages
                .entry((*from, *to))
                .or_default()
                .push(msg.id);
        }
    }

    // Verify each pair maintained FIFO
    for ((from, to), sent_order) in per_pair_messages {
        let received_order = system.get_received_order(from, to).await?;

        // Check that received is a subsequence of sent (allowing drops)
        let mut sent_iter = sent_order.iter();
        for received_id in &received_order {
            loop {
                match sent_iter.next() {
                    Some(sent_id) if sent_id == received_id => break,
                    Some(_) => continue, // Skip (message was dropped)
                    None => {
                        always_assert!(
                            fifo_violation_detected,
                            false,
                            format!("FIFO violation: {} -> {}, received {:?} out of order",
                                    from, to, received_id)
                        );
                    }
                }
            }
        }
    }

    Ok(())
}
```

#### Transaction Serializability

```rust
#[derive(Clone, Debug)]
struct TransactionLog {
    txn_id: TransactionId,
    operations: Vec<TxnOp>,
    start_time: Instant,
    end_time: Option<Instant>,
    outcome: Option<TxnOutcome>,
}

async fn execute_transaction_workload(
    random: SimRandomProvider,
    system: &TransactionSystem,
) -> Result<Vec<TransactionLog>> {
    let mut log = Vec::new();

    // Execute many transactions
    for _ in 0..500 {
        let txn_id = system.begin_transaction().await?;
        let mut txn_log = TransactionLog {
            txn_id,
            operations: vec![],
            start_time: Instant::now(),
            end_time: None,
            outcome: None,
        };

        // Random operations within transaction
        for _ in 0..random.range(1..10) {
            let op = generate_random_txn_op(&random);
            txn_log.operations.push(op.clone());
            system.execute_txn_op(txn_id, op).await?;
        }

        // Commit or abort
        let outcome = if random.random_bool(0.9) {
            system.commit(txn_id).await
        } else {
            system.abort(txn_id).await
        };

        txn_log.end_time = Some(Instant::now());
        txn_log.outcome = Some(outcome);

        log.push(txn_log);
    }

    Ok(log)
}

async fn verify_serializability(
    log: &[TransactionLog],
    system: &TransactionSystem,
) -> Result<()> {
    // Build conflict graph
    let mut conflict_graph = HashMap::new();

    for (i, txn_a) in log.iter().enumerate() {
        for (j, txn_b) in log.iter().enumerate().skip(i + 1) {
            if has_conflict(txn_a, txn_b) {
                conflict_graph.entry(txn_a.txn_id)
                    .or_insert_with(Vec::new)
                    .push(txn_b.txn_id);
            }
        }
    }

    // Check for cycles (would violate serializability)
    let has_cycle = detect_cycle(&conflict_graph);

    always_assert!(
        serializable_schedule,
        !has_cycle,
        "Non-serializable schedule detected - conflict graph has cycle"
    );

    Ok(())
}
```

### Advantages

- **Detailed debugging**: See exact sequence that caused failure
- **Post-hoc analysis**: Analyze logs after test completes
- **Complex properties**: Can verify ordering, causality, etc.

### Disadvantages

- **Memory overhead**: Logs can grow large
- **Analysis complexity**: Verification code can be complex
- **Delayed feedback**: Only detect issues at end

---

## Pattern 3: Invariant Tracking

### When to Use

- System has mathematical properties that must hold
- Properties can be checked efficiently
- Want continuous validation during execution

### Examples

#### Banking: Money Conservation

```rust
struct BankInvariants {
    initial_balance: u64,
    total_deposits: AtomicU64,
    total_withdrawals: AtomicU64,
}

impl BankInvariants {
    fn new(initial: u64) -> Self {
        Self {
            initial_balance: initial,
            total_deposits: AtomicU64::new(0),
            total_withdrawals: AtomicU64::new(0),
        }
    }

    fn record_deposit(&self, amount: u64) {
        self.total_deposits.fetch_add(amount, Ordering::SeqCst);
    }

    fn record_withdrawal(&self, amount: u64) {
        self.total_withdrawals.fetch_add(amount, Ordering::SeqCst);
    }

    fn verify(&self, accounts: &HashMap<AccountId, u64>) -> Result<()> {
        let current_total: u64 = accounts.values().sum();
        let deposits = self.total_deposits.load(Ordering::SeqCst);
        let withdrawals = self.total_withdrawals.load(Ordering::SeqCst);

        let expected = self.initial_balance + deposits - withdrawals;

        always_assert!(
            money_conservation,
            current_total == expected,
            format!(
                "Money conservation violated: current={}, expected={}, \
                 initial={}, deposits={}, withdrawals={}",
                current_total, expected, self.initial_balance,
                deposits, withdrawals
            )
        );

        Ok(())
    }
}
```

#### Distributed Counter: Value Bounds

```rust
struct CounterInvariants {
    min_value: AtomicI64,
    max_value: AtomicI64,
    increment_count: AtomicU64,
    decrement_count: AtomicU64,
}

impl CounterInvariants {
    fn new() -> Self {
        Self {
            min_value: AtomicI64::new(0),
            max_value: AtomicI64::new(0),
            increment_count: AtomicU64::new(0),
            decrement_count: AtomicU64::new(0),
        }
    }

    fn record_increment(&self, delta: i64) {
        self.increment_count.fetch_add(1, Ordering::SeqCst);
        self.max_value.fetch_add(delta, Ordering::SeqCst);
    }

    fn record_decrement(&self, delta: i64) {
        self.decrement_count.fetch_add(1, Ordering::SeqCst);
        self.min_value.fetch_sub(delta, Ordering::SeqCst);
    }

    fn verify(&self, actual_value: i64) -> Result<()> {
        let min_possible = self.min_value.load(Ordering::SeqCst);
        let max_possible = self.max_value.load(Ordering::SeqCst);

        always_assert!(
            value_within_bounds,
            actual_value >= min_possible && actual_value <= max_possible,
            format!(
                "Counter value {} outside possible bounds [{}, {}]",
                actual_value, min_possible, max_possible
            )
        );

        Ok(())
    }
}
```

#### Message Bus: Conservation Law

```rust
struct MessageBusInvariants {
    messages_sent: AtomicU64,
    messages_received: AtomicU64,
    messages_in_flight: AtomicU64,
}

impl MessageBusInvariants {
    fn record_send(&self) {
        self.messages_sent.fetch_add(1, Ordering::SeqCst);
        self.messages_in_flight.fetch_add(1, Ordering::SeqCst);
    }

    fn record_receive(&self) {
        self.messages_received.fetch_add(1, Ordering::SeqCst);
        self.messages_in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    fn record_drop(&self) {
        self.messages_in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    fn verify(&self) -> Result<()> {
        let sent = self.messages_sent.load(Ordering::SeqCst);
        let received = self.messages_received.load(Ordering::SeqCst);
        let in_flight = self.messages_in_flight.load(Ordering::SeqCst);

        // Conservation: sent = received + in_flight + dropped
        always_assert!(
            message_conservation,
            received + in_flight <= sent,
            format!(
                "Message conservation violated: sent={}, received={}, in_flight={}",
                sent, received, in_flight
            )
        );

        // No negative in-flight
        always_assert!(
            positive_in_flight,
            in_flight >= 0,
            format!("Negative in-flight count: {}", in_flight)
        );

        Ok(())
    }
}
```

#### Graph Topology: Cycle Detection

```rust
struct GraphInvariants {
    edges: Vec<(NodeId, NodeId)>,
}

impl GraphInvariants {
    fn add_edge(&mut self, from: NodeId, to: NodeId) {
        self.edges.push((from, to));
    }

    fn verify_acyclic(&self) -> Result<()> {
        let has_cycle = self.detect_cycle();

        always_assert!(
            graph_acyclic,
            !has_cycle,
            "Cycle detected in graph that should be acyclic"
        );

        Ok(())
    }

    fn detect_cycle(&self) -> bool {
        // Build adjacency list
        let mut adj: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for (from, to) in &self.edges {
            adj.entry(*from).or_default().push(*to);
        }

        // DFS cycle detection
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();

        for node in adj.keys() {
            if !visited.contains(node) {
                if self.has_cycle_dfs(*node, &adj, &mut visited, &mut rec_stack) {
                    return true;
                }
            }
        }

        false
    }

    fn has_cycle_dfs(
        &self,
        node: NodeId,
        adj: &HashMap<NodeId, Vec<NodeId>>,
        visited: &mut HashSet<NodeId>,
        rec_stack: &mut HashSet<NodeId>,
    ) -> bool {
        visited.insert(node);
        rec_stack.insert(node);

        if let Some(neighbors) = adj.get(&node) {
            for neighbor in neighbors {
                if !visited.contains(neighbor) {
                    if self.has_cycle_dfs(*neighbor, adj, visited, rec_stack) {
                        return true;
                    }
                } else if rec_stack.contains(neighbor) {
                    return true;
                }
            }
        }

        rec_stack.remove(&node);
        false
    }
}
```

### Advantages

- **Continuous validation**: Detect violations immediately
- **Efficient**: Properties checked incrementally
- **Mathematical certainty**: Based on provable properties

### Disadvantages

- **Requires math insight**: Must identify invariants
- **Limited scope**: Not all correctness is expressible as invariants
- **Tracking overhead**: Need to maintain invariant state

---

## Combining Verification Patterns

Often best to use multiple patterns together:

```rust
struct CombinedVerifier {
    // Pattern 1: Reference for simple state
    reference_kv: HashMap<String, Vec<u8>>,

    // Pattern 2: Operation log for analysis
    operation_log: Vec<LoggedOp>,

    // Pattern 3: Invariants for continuous checking
    invariants: SystemInvariants,
}

async fn execute_with_verification(
    op: Operation,
    verifier: &mut CombinedVerifier,
    system: &System,
) -> Result<()> {
    // Log operation (Pattern 2)
    verifier.operation_log.push(LoggedOp::new(op.clone()));

    // Execute and verify against reference (Pattern 1)
    match op {
        Operation::Put(key, value) => {
            verifier.reference_kv.insert(key.clone(), value.clone());
            system.put(key, value).await?;
        }
        Operation::Get(key) => {
            let expected = verifier.reference_kv.get(&key);
            let actual = system.get(&key).await?;

            always_assert!(
                get_matches_reference,
                actual.as_ref() == expected,
                "Get mismatch with reference"
            );
        }
    }

    // Verify invariants (Pattern 3)
    verifier.invariants.verify(system).await?;

    Ok(())
}
```

---

## Pattern Selection Guide

| System Type | Recommended Pattern | Reason |
|------------|-------------------|---------|
| Key-Value Store | Reference Implementation | Simple reference (HashMap) available |
| Distributed Counter | Invariant Tracking | Clear bounds and conservation laws |
| Transaction System | Operation Logging | Need serializability analysis |
| Banking System | Invariant Tracking | Money conservation property |
| Message Routing | Invariant Tracking + Logging | Conservation law + ordering analysis |
| Actor Directory | Reference Implementation | Simple HashMap reference |
| Consensus Protocol | Operation Logging | Complex agreement properties |
| Queue System | Invariant Tracking | Size bounds and ordering |

---

## Implementation Checklist

For each verification pattern:

**Reference Implementation**:
- [ ] Identify simple reference (HashMap, Vec, etc.)
- [ ] Mirror all operations to reference
- [ ] Compare results after each read operation
- [ ] Handle edge cases (not found, etc.)

**Operation Logging**:
- [ ] Define operation log structure
- [ ] Record operations with timestamps
- [ ] Implement post-execution analysis
- [ ] Check ordering, causality, serializability

**Invariant Tracking**:
- [ ] Identify mathematical properties
- [ ] Implement tracking structure (atomic counters, etc.)
- [ ] Update tracking on each operation
- [ ] Verify invariants frequently (after each op or periodically)
- [ ] Use always_assert! for violations
