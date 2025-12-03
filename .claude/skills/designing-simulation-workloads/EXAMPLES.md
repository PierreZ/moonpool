# Workload Examples

This file contains three complete autonomous workload examples demonstrating different testing patterns and verification strategies.

## Example 1: Bank Account Workload

**Purpose**: Test transactional operations with money conservation invariant

**Verification Pattern**: Invariant tracking (balance conservation)

### Operation Alphabet

```rust
enum BankOp {
    CreateAccount(AccountId, u64),    // id, initial_balance
    Deposit(AccountId, u64),           // id, amount
    Withdraw(AccountId, u64),          // id, amount
    Transfer(AccountId, AccountId, u64), // from, to, amount
    CheckBalance(AccountId),
    CloseAccount(AccountId),
}
```

### Complete Workload Implementation

```rust
async fn bank_account_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let runtime = ActorRuntime::with_providers(
        "bank",
        network,
        time,
        task_provider.clone(),
    ).await?;

    let num_ops = 1000;
    let account_ids: Vec<_> = (0..20)
        .map(|i| format!("account_{}", i))
        .collect();

    let mut operations = vec![];
    for _ in 0..num_ops {
        let op = match random.range(0..6) {
            0 => BankOp::CreateAccount(
                random.choice(&account_ids).clone(),
                random.range(0..10000),
            ),
            1 => BankOp::Deposit(
                random.choice(&account_ids).clone(),
                random.range(1..1000),
            ),
            2 => BankOp::Withdraw(
                random.choice(&account_ids).clone(),
                random.range(1..1000),
            ),
            3 => BankOp::Transfer(
                random.choice(&account_ids).clone(),
                random.choice(&account_ids).clone(),
                random.range(1..1000),
            ),
            4 => BankOp::CheckBalance(
                random.choice(&account_ids).clone(),
            ),
            5 => BankOp::CloseAccount(
                random.choice(&account_ids).clone(),
            ),
            _ => unreachable!(),
        };
        operations.push(op);
    }

    let mut tasks = vec![];
    for op in operations {
        let runtime_clone = runtime.clone();
        let task = task_provider.spawn_task(async move {
            execute_bank_op(op, &runtime_clone).await
        });
        tasks.push(task);
    }

    for task in tasks {
        let _ = task.await;
    }

    validate_bank_invariants(&runtime).await;

    Ok(SimulationMetrics::default())
}
```

### Operation Execution

```rust
async fn execute_bank_op(
    op: BankOp,
    runtime: &ActorRuntime,
) -> Result<()> {
    match op {
        BankOp::CreateAccount(id, balance) => {
            runtime.get_actor::<BankAccount>("BankAccount", &id)
                .call(CreateAccountMsg { initial_balance: balance })
                .await?;

            sometimes_assert!(
                account_created,
                true,
                "Bank account created"
            );
        }
        BankOp::Deposit(id, amount) => {
            runtime.get_actor::<BankAccount>("BankAccount", &id)
                .call(DepositMsg { amount })
                .await?;

            sometimes_assert!(
                deposit_succeeded,
                amount > 0,
                "Deposit with positive amount"
            );
        }
        BankOp::Withdraw(id, amount) => {
            match runtime.get_actor::<BankAccount>("BankAccount", &id)
                .call(WithdrawMsg { amount })
                .await {
                Ok(_) => {
                    sometimes_assert!(
                        withdrawal_succeeded,
                        true,
                        "Withdrawal succeeded"
                    );
                }
                Err(BankError::InsufficientFunds) => {
                    sometimes_assert!(
                        insufficient_funds_caught,
                        true,
                        "Insufficient funds error triggered"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        BankOp::Transfer(from, to, amount) => {
            match runtime.get_actor::<BankAccount>("BankAccount", &from)
                .call(TransferMsg { to_account: to, amount })
                .await {
                Ok(_) => {
                    sometimes_assert!(
                        transfer_succeeded,
                        true,
                        "Transfer completed"
                    );
                }
                Err(_) => {
                    sometimes_assert!(
                        transfer_failed,
                        true,
                        "Transfer failed (insufficient funds or invalid account)"
                    );
                }
            }
        }
        BankOp::CheckBalance(id) => {
            let balance = runtime.get_actor::<BankAccount>("BankAccount", &id)
                .call(GetBalanceMsg {})
                .await?;

            always_assert!(
                balance_non_negative,
                balance >= 0,
                "Account balance is negative"
            );

            sometimes_assert!(
                balance_checked,
                true,
                "Balance check completed"
            );
        }
        BankOp::CloseAccount(id) => {
            match runtime.get_actor::<BankAccount>("BankAccount", &id)
                .call(CloseAccountMsg {})
                .await {
                Ok(_) => {
                    sometimes_assert!(
                        account_closed,
                        true,
                        "Account closed successfully"
                    );
                }
                Err(_) => {
                    sometimes_assert!(
                        account_close_failed,
                        true,
                        "Account close failed (non-zero balance or doesn't exist)"
                    );
                }
            }
        }
    }
    Ok(())
}
```

### Invariant Validation

```rust
async fn validate_bank_invariants(runtime: &ActorRuntime) {
    let total_deposits = get_total_deposits(runtime).await;
    let total_withdrawals = get_total_withdrawals(runtime).await;
    let current_balances = get_all_balances(runtime).await;

    // Money conservation law
    always_assert!(
        money_conservation,
        current_balances == total_deposits - total_withdrawals,
        format!(
            "Money conservation violated: balances={}, deposits={}, withdrawals={}",
            current_balances, total_deposits, total_withdrawals
        )
    );

    // All balances non-negative
    for (account_id, balance) in get_individual_balances(runtime).await {
        always_assert!(
            balance_non_negative,
            balance >= 0,
            format!("Account {} has negative balance: {}", account_id, balance)
        );
    }
}
```

**What This Tests**:
- Concurrent deposits and withdrawals
- Transfer atomicity
- Insufficient funds handling
- Account lifecycle (create/close)
- Money conservation under chaos

---

## Example 2: Directory Consistency Workload

**Purpose**: Test actor placement and directory consistency across nodes

**Verification Pattern**: Invariant tracking (single location property)

### Operation Alphabet

```rust
enum DirectoryOp {
    ActivateActor(ActorId, NodeId),
    DeactivateActor(ActorId),
    MigrateActor(ActorId, NodeId),
    LookupActor(ActorId),
    CrashNode(NodeId),
}
```

### Complete Workload Implementation

```rust
async fn directory_chaos_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let runtime = ActorRuntime::with_providers(
        "directory_test",
        network,
        time,
        task_provider.clone(),
    ).await?;

    let actor_ids: Vec<_> = (0..50)
        .map(|i| ActorId::virtual_actor("TestActor", &format!("actor_{}", i)))
        .collect();

    let node_ids = vec![NodeId(1), NodeId(2), NodeId(3)];

    let mut operations = vec![];
    for _ in 0..2000 {
        let op = match random.range(0..5) {
            0 => DirectoryOp::ActivateActor(
                random.choice(&actor_ids).clone(),
                random.choice(&node_ids).clone(),
            ),
            1 => DirectoryOp::DeactivateActor(
                random.choice(&actor_ids).clone(),
            ),
            2 => DirectoryOp::MigrateActor(
                random.choice(&actor_ids).clone(),
                random.choice(&node_ids).clone(),
            ),
            3 => DirectoryOp::LookupActor(
                random.choice(&actor_ids).clone(),
            ),
            4 => DirectoryOp::CrashNode(
                random.choice(&node_ids).clone(),
            ),
            _ => unreachable!(),
        };
        operations.push(op);
    }

    let mut tasks = vec![];
    for op in operations {
        let runtime_clone = runtime.clone();
        let task = task_provider.spawn_task(async move {
            // Validate before operation
            validate_directory_invariant(&runtime_clone).await;

            execute_directory_op(op, &runtime_clone).await;

            // Validate after operation
            validate_directory_invariant(&runtime_clone).await;
        });
        tasks.push(task);

        if buggify!() {
            time.sleep(Duration::from_millis(random.range(1..50))).await;
        }
    }

    for task in tasks {
        let _ = task.await;
    }

    Ok(SimulationMetrics::default())
}
```

### Operation Execution

```rust
async fn execute_directory_op(
    op: DirectoryOp,
    runtime: &ActorRuntime,
) -> Result<()> {
    match op {
        DirectoryOp::ActivateActor(actor_id, node_id) => {
            runtime.directory().register(&actor_id, node_id).await?;

            sometimes_assert!(
                actor_registered,
                true,
                "Actor registered in directory"
            );
        }
        DirectoryOp::DeactivateActor(actor_id) => {
            match runtime.directory().unregister(&actor_id).await {
                Ok(_) => {
                    sometimes_assert!(
                        actor_unregistered,
                        true,
                        "Actor unregistered from directory"
                    );
                }
                Err(DirectoryError::NotFound) => {
                    sometimes_assert!(
                        unregister_not_found,
                        true,
                        "Unregister called on non-existent actor"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        DirectoryOp::MigrateActor(actor_id, new_node) => {
            runtime.directory().unregister(&actor_id).await.ok();
            runtime.directory().register(&actor_id, new_node).await?;

            sometimes_assert!(
                actor_migrated,
                true,
                "Actor migrated to new node"
            );
        }
        DirectoryOp::LookupActor(actor_id) => {
            match runtime.directory().lookup(&actor_id).await {
                Ok(Some(node_id)) => {
                    sometimes_assert!(
                        lookup_succeeded,
                        true,
                        "Actor lookup found location"
                    );
                }
                Ok(None) => {
                    sometimes_assert!(
                        lookup_not_found,
                        true,
                        "Actor lookup returned no location"
                    );
                }
                Err(_) => {
                    sometimes_assert!(
                        lookup_failed,
                        true,
                        "Actor lookup failed"
                    );
                }
            }
        }
        DirectoryOp::CrashNode(node_id) => {
            runtime.crash_node(node_id).await;

            sometimes_assert!(
                node_crashed,
                true,
                "Node crash simulated"
            );
        }
    }
    Ok(())
}
```

### Invariant Validation

```rust
async fn validate_directory_invariant(runtime: &ActorRuntime) {
    let directory = runtime.directory();
    let all_actors = directory.get_all_actors().await;

    for actor_id in all_actors {
        let locations = directory.lookup_all(&actor_id).await.unwrap();

        // Core invariant: Each actor in at most one location
        always_assert!(
            single_location,
            locations.len() <= 1,
            format!("Actor {:?} appears in {} locations: {:?}",
                    actor_id, locations.len(), locations)
        );
    }
}
```

**What This Tests**:
- Concurrent actor activation
- Directory registration race conditions
- Actor migration correctness
- Lookup consistency
- Node crash recovery

---

## Example 3: MessageBus Routing Chaos Workload

**Purpose**: Test message routing and delivery across nodes

**Verification Pattern**: Invariant tracking (message conservation)

### Operation Alphabet

```rust
enum RoutingOp {
    SendMessage(ActorId, Message),
    SendRequest(ActorId, Request),
    ActivateRemoteActor(ActorId, NodeId),
    PartitionNetwork(NodeId, NodeId),
    HealPartition(NodeId, NodeId),
    CrashMessageBus(NodeId),
    RestoreMessageBus(NodeId),
}
```

### Complete Workload Implementation

```rust
async fn routing_chaos_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let nodes = vec![
        create_runtime("node_1", "127.0.0.1:5001",
                       network.clone(), time.clone(), task_provider.clone()).await?,
        create_runtime("node_2", "127.0.0.1:5002",
                       network.clone(), time.clone(), task_provider.clone()).await?,
        create_runtime("node_3", "127.0.0.1:5003",
                       network.clone(), time.clone(), task_provider.clone()).await?,
    ];

    let actor_ids: Vec<_> = (0..30)
        .map(|i| ActorId::virtual_actor("Router", &format!("router_{}", i)))
        .collect();

    let mut operations = vec![];
    for _ in 0..1500 {
        let op = match random.range(0..7) {
            0 | 1 | 2 => RoutingOp::SendMessage(
                random.choice(&actor_ids).clone(),
                random_message(),
            ),
            3 => RoutingOp::SendRequest(
                random.choice(&actor_ids).clone(),
                random_request(),
            ),
            4 => RoutingOp::PartitionNetwork(
                random.choice(&[NodeId(1), NodeId(2), NodeId(3)]),
                random.choice(&[NodeId(1), NodeId(2), NodeId(3)]),
            ),
            5 => RoutingOp::HealPartition(
                random.choice(&[NodeId(1), NodeId(2), NodeId(3)]),
                random.choice(&[NodeId(1), NodeId(2), NodeId(3)]),
            ),
            6 => RoutingOp::ActivateRemoteActor(
                random.choice(&actor_ids).clone(),
                random.choice(&[NodeId(1), NodeId(2), NodeId(3)]),
            ),
            _ => unreachable!(),
        };
        operations.push(op);
    }

    let mut tasks = vec![];
    for op in operations {
        let nodes_clone = nodes.clone();
        let task = task_provider.spawn_task(async move {
            execute_routing_op(op, &nodes_clone).await;

            sometimes_assert!(
                messages_routed,
                true,
                "Message routing attempted"
            );
        });
        tasks.push(task);
    }

    for task in tasks {
        let _ = task.await;
    }

    validate_message_conservation(&nodes).await;

    Ok(SimulationMetrics::default())
}
```

### Operation Execution

```rust
async fn execute_routing_op(
    op: RoutingOp,
    nodes: &[ActorRuntime],
) -> Result<()> {
    match op {
        RoutingOp::SendMessage(actor_id, msg) => {
            let sender_node = random.choice(nodes);

            match sender_node.send_message(actor_id, msg).await {
                Ok(_) => {
                    sometimes_assert!(
                        message_sent,
                        true,
                        "Message sent successfully"
                    );
                }
                Err(_) => {
                    sometimes_assert!(
                        message_send_failed,
                        true,
                        "Message send failed (actor not found or network issue)"
                    );
                }
            }
        }
        RoutingOp::SendRequest(actor_id, request) => {
            let sender_node = random.choice(nodes);

            match sender_node.send_request(actor_id, request).await {
                Ok(response) => {
                    sometimes_assert!(
                        request_succeeded,
                        true,
                        "Request-response completed"
                    );
                }
                Err(_) => {
                    sometimes_assert!(
                        request_failed,
                        true,
                        "Request failed (timeout or actor unavailable)"
                    );
                }
            }
        }
        RoutingOp::ActivateRemoteActor(actor_id, node_id) => {
            let target_node = nodes.iter()
                .find(|n| n.node_id() == node_id)
                .unwrap();

            target_node.activate_actor(actor_id).await?;

            sometimes_assert!(
                remote_actor_activated,
                true,
                "Remote actor activated"
            );
        }
        RoutingOp::PartitionNetwork(node1, node2) => {
            for node in nodes {
                node.network().partition(node1, node2).await;
            }

            sometimes_assert!(
                network_partitioned,
                true,
                "Network partition created"
            );
        }
        RoutingOp::HealPartition(node1, node2) => {
            for node in nodes {
                node.network().heal_partition(node1, node2).await;
            }

            sometimes_assert!(
                partition_healed,
                true,
                "Network partition healed"
            );
        }
        RoutingOp::CrashMessageBus(node_id) => {
            let target_node = nodes.iter()
                .find(|n| n.node_id() == node_id)
                .unwrap();

            target_node.message_bus().crash().await;

            sometimes_assert!(
                bus_crashed,
                true,
                "MessageBus crashed"
            );
        }
        RoutingOp::RestoreMessageBus(node_id) => {
            let target_node = nodes.iter()
                .find(|n| n.node_id() == node_id)
                .unwrap();

            target_node.message_bus().restore().await;

            sometimes_assert!(
                bus_restored,
                true,
                "MessageBus restored"
            );
        }
    }
    Ok(())
}
```

### Invariant Validation

```rust
async fn validate_message_conservation(nodes: &[ActorRuntime]) {
    let total_sent: u64 = nodes.iter()
        .map(|n| n.message_bus().metrics().messages_sent)
        .sum();

    let total_received: u64 = nodes.iter()
        .map(|n| n.message_bus().metrics().messages_received)
        .sum();

    // Messages can be lost due to network issues, but never duplicated
    always_assert!(
        message_conservation,
        total_received <= total_sent,
        format!(
            "Message conservation violated: sent={}, received={}",
            total_sent, total_received
        )
    );

    // Check per-node metrics
    for node in nodes {
        let metrics = node.message_bus().metrics();

        always_assert!(
            node_conservation,
            metrics.messages_received <= metrics.messages_sent + metrics.messages_forwarded,
            format!(
                "Node {:?} conservation violated: received={}, sent={}, forwarded={}",
                node.node_id(),
                metrics.messages_received,
                metrics.messages_sent,
                metrics.messages_forwarded
            )
        );
    }
}
```

**What This Tests**:
- Multi-node message routing
- Request-response across nodes
- Network partition handling
- MessageBus crash recovery
- Message conservation under chaos
- Remote actor activation

---

## Usage Notes

These examples demonstrate:
- Different verification patterns (invariant tracking)
- Various operation alphabets (banking, directory, routing)
- Concurrent execution (hundreds to thousands of operations)
- Strategic assertion placement
- Final state validation

To adapt for your use case:
1. Define your operation enum
2. Implement execute_operation()
3. Add appropriate assertions
4. Design verification strategy
5. Scale from simple (100 ops) to complex (2000 ops)
