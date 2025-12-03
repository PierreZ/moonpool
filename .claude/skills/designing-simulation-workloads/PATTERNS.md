# Operation Alphabet Patterns

This file provides detailed implementations of operation alphabet patterns for different system types.

## Pattern 1: Actor Lifecycle Operations

For testing actor activation, deactivation, and state management.

```rust
enum ActorLifecycleOp {
    // Basic lifecycle
    Activate(ActorId),
    Deactivate(ActorId),
    Reactivate(ActorId),

    // State operations
    SaveState(ActorId),
    LoadState(ActorId),
    ClearState(ActorId),

    // Health checks
    Ping(ActorId),
    GetStatus(ActorId),
}

async fn execute_lifecycle_op(
    op: ActorLifecycleOp,
    runtime: &ActorRuntime,
) -> Result<()> {
    match op {
        ActorLifecycleOp::Activate(id) => {
            runtime.activate_actor(id.clone()).await?;

            sometimes_assert!(
                actor_activated,
                true,
                "Actor activation succeeded"
            );
        }
        ActorLifecycleOp::Deactivate(id) => {
            runtime.deactivate_actor(&id).await?;

            sometimes_assert!(
                actor_deactivated,
                true,
                "Actor deactivation succeeded"
            );

            // Verify cleanup
            always_assert!(
                resources_released,
                !runtime.has_active_actor(&id),
                "Actor still active after deactivation"
            );
        }
        ActorLifecycleOp::Reactivate(id) => {
            runtime.deactivate_actor(&id).await.ok();
            runtime.activate_actor(id).await?;

            sometimes_assert!(
                actor_reactivated,
                true,
                "Actor reactivation (deactivate + activate) succeeded"
            );
        }
        ActorLifecycleOp::SaveState(id) => {
            runtime.get_actor(&id)?.save_state().await?;

            sometimes_assert!(
                state_saved,
                true,
                "Actor state saved"
            );
        }
        ActorLifecycleOp::LoadState(id) => {
            match runtime.get_actor(&id)?.load_state().await {
                Ok(_) => {
                    sometimes_assert!(
                        state_loaded,
                        true,
                        "Actor state loaded"
                    );
                }
                Err(StateError::NotFound) => {
                    sometimes_assert!(
                        state_not_found,
                        true,
                        "State load attempted on non-existent state"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        // ... other operations
    }
    Ok(())
}
```

**Usage**: Tests concurrent activation, state persistence, resource cleanup

---

## Pattern 2: Messaging Operations

For testing message delivery, request-response, and routing.

```rust
enum MessagingOp {
    // One-way messaging
    SendMessage(ActorId, Message),
    BroadcastMessage(Vec<ActorId>, Message),

    // Request-response
    SendRequest(ActorId, Request),
    SendRequestWithTimeout(ActorId, Request, Duration),

    // Routing
    RouteViaDirectory(ActorId, Message),
    RouteDirectly(NodeId, ActorId, Message),
}

async fn execute_messaging_op(
    op: MessagingOp,
    runtime: &ActorRuntime,
) -> Result<()> {
    match op {
        MessagingOp::SendMessage(target, msg) => {
            runtime.send_message(target, msg).await?;

            sometimes_assert!(
                message_sent,
                true,
                "Message sent successfully"
            );
        }
        MessagingOp::BroadcastMessage(targets, msg) => {
            for target in &targets {
                runtime.send_message(target.clone(), msg.clone()).await.ok();
            }

            sometimes_assert!(
                broadcast_sent,
                targets.len() > 1,
                "Broadcast sent to multiple targets"
            );
        }
        MessagingOp::SendRequest(target, request) => {
            match runtime.send_request(target, request).await {
                Ok(response) => {
                    sometimes_assert!(
                        request_completed,
                        true,
                        "Request-response completed"
                    );
                }
                Err(RequestError::Timeout) => {
                    sometimes_assert!(
                        request_timeout,
                        true,
                        "Request timed out"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        MessagingOp::SendRequestWithTimeout(target, request, timeout) => {
            match time.timeout(timeout, runtime.send_request(target, request)).await {
                Ok(Ok(response)) => {
                    sometimes_assert!(
                        request_within_timeout,
                        true,
                        "Request completed within timeout"
                    );
                }
                Ok(Err(_)) => {
                    sometimes_assert!(
                        request_failed,
                        true,
                        "Request failed"
                    );
                }
                Err(_) => {
                    sometimes_assert!(
                        timeout_triggered,
                        true,
                        "Timeout triggered"
                    );
                }
            }
        }
        // ... other operations
    }
    Ok(())
}
```

**Usage**: Tests message delivery, timeout handling, broadcast scenarios

---

## Pattern 3: Network Chaos Operations

For testing behavior under network failures and partitions.

```rust
enum NetworkChaosOp {
    // Connection control
    OpenConnection(NodeId, NodeId),
    CloseConnection(NodeId, NodeId),
    ResetConnection(NodeId, NodeId),

    // Network partitions
    CreatePartition(Vec<NodeId>, Vec<NodeId>),
    HealPartition,

    // Failure injection
    DropPackets(NodeId, f64), // Drop percentage
    DelayPackets(NodeId, Duration),
    CorruptPackets(NodeId, f64),

    // Recovery
    RestoreNetwork,
}

async fn execute_network_chaos_op(
    op: NetworkChaosOp,
    network: &SimNetworkProvider,
) -> Result<()> {
    match op {
        NetworkChaosOp::OpenConnection(from, to) => {
            network.open_connection(from, to).await;

            sometimes_assert!(
                connection_opened,
                true,
                "Connection opened"
            );
        }
        NetworkChaosOp::CloseConnection(from, to) => {
            network.close_connection(from, to).await;

            sometimes_assert!(
                connection_closed,
                true,
                "Connection closed"
            );
        }
        NetworkChaosOp::CreatePartition(group_a, group_b) => {
            for node_a in &group_a {
                for node_b in &group_b {
                    network.partition(*node_a, *node_b).await;
                }
            }

            sometimes_assert!(
                partition_created,
                group_a.len() > 0 && group_b.len() > 0,
                "Network partition created between groups"
            );
        }
        NetworkChaosOp::HealPartition => {
            network.heal_all_partitions().await;

            sometimes_assert!(
                partition_healed,
                true,
                "All network partitions healed"
            );
        }
        NetworkChaosOp::DropPackets(node, percentage) => {
            network.set_packet_drop_rate(node, percentage).await;

            sometimes_assert!(
                packet_drop_enabled,
                percentage > 0.0,
                "Packet drop enabled"
            );
        }
        // ... other operations
    }
    Ok(())
}
```

**Usage**: Tests network resilience, partition tolerance, connection management

---

## Pattern 4: Resource Contention Operations

For testing behavior under resource pressure.

```rust
enum ResourceContentionOp {
    // Queue operations
    FillQueue(ActorId, usize), // Fill to N% capacity
    DrainQueue(ActorId),
    OverflowQueue(ActorId),

    // Memory pressure
    AllocateLargeState(ActorId, usize),
    FreeState(ActorId),

    // CPU pressure
    BusyWait(ActorId, Duration),
    YieldExecution(ActorId),

    // Connection limits
    OpenMaxConnections(NodeId),
    ExhaustConnectionPool(NodeId),
}

async fn execute_resource_contention_op(
    op: ResourceContentionOp,
    runtime: &ActorRuntime,
) -> Result<()> {
    match op {
        ResourceContentionOp::FillQueue(actor_id, target_size) => {
            for i in 0..target_size {
                runtime.send_message(
                    actor_id.clone(),
                    Message::Filler(i)
                ).await.ok();
            }

            sometimes_assert!(
                queue_filled,
                target_size > 100,
                "Queue filled with many messages"
            );
        }
        ResourceContentionOp::OverflowQueue(actor_id) => {
            // Attempt to send beyond capacity
            for i in 0..10000 {
                match runtime.send_message(actor_id.clone(), Message::Filler(i)).await {
                    Ok(_) => {},
                    Err(MessageError::QueueFull) => {
                        sometimes_assert!(
                            queue_overflow_caught,
                            true,
                            "Queue overflow detected"
                        );
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        ResourceContentionOp::AllocateLargeState(actor_id, size_mb) => {
            runtime.get_actor(&actor_id)?
                .allocate_state(size_mb * 1024 * 1024)
                .await?;

            sometimes_assert!(
                large_state_allocated,
                size_mb > 10,
                "Large state allocation succeeded"
            );
        }
        // ... other operations
    }
    Ok(())
}
```

**Usage**: Tests queue management, memory limits, resource exhaustion

---

## Pattern 5: Time-Based Operations

For testing timeouts, delays, and scheduling.

```rust
enum TimeBasedOp {
    // Delays
    Sleep(Duration),
    RandomDelay(Duration, Duration), // min, max

    // Timeouts
    OperationWithTimeout(Box<Operation>, Duration),
    CancelAfterDelay(TaskHandle, Duration),

    // Scheduling
    ScheduleAt(Instant, Box<Operation>),
    ScheduleRepeating(Duration, Box<Operation>),

    // Time manipulation (simulation only)
    AdvanceTime(Duration),
}

async fn execute_time_based_op(
    op: TimeBasedOp,
    time: &SimTimeProvider,
    task_provider: &TokioTaskProvider,
) -> Result<()> {
    match op {
        TimeBasedOp::Sleep(duration) => {
            time.sleep(duration).await;

            sometimes_assert!(
                sleep_completed,
                true,
                "Sleep completed"
            );
        }
        TimeBasedOp::RandomDelay(min, max) => {
            let delay = random.range(min.as_millis()..max.as_millis());
            time.sleep(Duration::from_millis(delay as u64)).await;

            sometimes_assert!(
                random_delay_completed,
                true,
                "Random delay completed"
            );
        }
        TimeBasedOp::OperationWithTimeout(inner_op, timeout) => {
            match time.timeout(timeout, execute_operation(*inner_op)).await {
                Ok(result) => {
                    sometimes_assert!(
                        operation_within_timeout,
                        true,
                        "Operation completed within timeout"
                    );
                    result?;
                }
                Err(_) => {
                    sometimes_assert!(
                        operation_timeout,
                        true,
                        "Operation timed out"
                    );
                }
            }
        }
        TimeBasedOp::CancelAfterDelay(handle, delay) => {
            time.sleep(delay).await;
            handle.cancel();

            sometimes_assert!(
                task_cancelled,
                true,
                "Task cancelled after delay"
            );
        }
        // ... other operations
    }
    Ok(())
}
```

**Usage**: Tests timeout handling, scheduling, time-dependent logic

---

## Pattern 6: Multi-Actor Coordination

For testing interactions between multiple actors.

```rust
enum CoordinationOp {
    // Synchronization
    WaitForAll(Vec<ActorId>),
    Barrier(Vec<ActorId>),

    // Leader election
    ElectLeader(Vec<ActorId>),
    StepDownLeader(ActorId),

    // Distributed transactions
    BeginTransaction(Vec<ActorId>),
    CommitTransaction(TransactionId),
    AbortTransaction(TransactionId),

    // Consensus
    ProposeValue(Vec<ActorId>, Value),
    VoteOnProposal(ActorId, ProposalId, bool),
}

async fn execute_coordination_op(
    op: CoordinationOp,
    runtime: &ActorRuntime,
) -> Result<()> {
    match op {
        CoordinationOp::WaitForAll(actors) => {
            let mut tasks = vec![];
            for actor_id in actors {
                let task = task_provider.spawn_task(async move {
                    runtime.wait_for_ready(actor_id).await
                });
                tasks.push(task);
            }

            for task in tasks {
                task.await?;
            }

            sometimes_assert!(
                all_actors_ready,
                true,
                "All actors reached ready state"
            );
        }
        CoordinationOp::BeginTransaction(participants) => {
            let txn_id = runtime.transaction_coordinator()
                .begin(participants.clone())
                .await?;

            sometimes_assert!(
                transaction_begun,
                participants.len() > 1,
                "Multi-actor transaction begun"
            );
        }
        CoordinationOp::CommitTransaction(txn_id) => {
            match runtime.transaction_coordinator().commit(txn_id).await {
                Ok(_) => {
                    sometimes_assert!(
                        transaction_committed,
                        true,
                        "Transaction committed successfully"
                    );
                }
                Err(TransactionError::ConflictDetected) => {
                    sometimes_assert!(
                        transaction_conflict,
                        true,
                        "Transaction conflict detected"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        // ... other operations
    }
    Ok(())
}
```

**Usage**: Tests coordination protocols, distributed transactions, consensus

---

## Combining Patterns

Real workloads often combine multiple patterns:

```rust
enum CombinedOp {
    // Actor operations
    Lifecycle(ActorLifecycleOp),
    Messaging(MessagingOp),

    // Infrastructure operations
    NetworkChaos(NetworkChaosOp),
    ResourceContention(ResourceContentionOp),
    TimeBased(TimeBasedOp),

    // Coordination operations
    Coordination(CoordinationOp),
}

async fn execute_combined_op(
    op: CombinedOp,
    runtime: &ActorRuntime,
    network: &SimNetworkProvider,
    time: &SimTimeProvider,
) -> Result<()> {
    match op {
        CombinedOp::Lifecycle(inner) => {
            execute_lifecycle_op(inner, runtime).await
        }
        CombinedOp::Messaging(inner) => {
            execute_messaging_op(inner, runtime).await
        }
        CombinedOp::NetworkChaos(inner) => {
            execute_network_chaos_op(inner, network).await
        }
        CombinedOp::ResourceContention(inner) => {
            execute_resource_contention_op(inner, runtime).await
        }
        CombinedOp::TimeBased(inner) => {
            execute_time_based_op(inner, time, &runtime.task_provider()).await
        }
        CombinedOp::Coordination(inner) => {
            execute_coordination_op(inner, runtime).await
        }
    }
}
```

**Benefits**: Comprehensive state space exploration combining multiple concerns

---

## Pattern Selection Guide

Choose patterns based on what you're testing:

| Testing Goal | Recommended Patterns |
|-------------|---------------------|
| Actor activation races | Lifecycle + NetworkChaos |
| Message delivery reliability | Messaging + NetworkChaos |
| Timeout handling | Messaging + TimeBased |
| Queue overflow | ResourceContention + Messaging |
| Network partition tolerance | NetworkChaos + Messaging + Coordination |
| Distributed transactions | Coordination + Lifecycle + NetworkChaos |
| Load balancing | Messaging + ResourceContention |
| State persistence | Lifecycle + NetworkChaos |

---

## Implementation Tips

1. **Start with 2-3 patterns**: Don't try to test everything at once
2. **Weight operation distribution**: Some operations should be more common
   ```rust
   let op = match random.range(0..10) {
       0..=6 => Operation::Common(/*...*/),  // 70%
       7..=8 => Operation::Uncommon(/*...*/), // 20%
       9 => Operation::Rare(/*...*/),         // 10%
       _ => unreachable!(),
   };
   ```
3. **Add chaos incrementally**: Start without NetworkChaos, add later
4. **Balance work and infrastructure ops**: 80% work, 20% chaos is a good starting point
