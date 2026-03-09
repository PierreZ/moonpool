# Designing Simulation-Friendly RPC

<!-- toc -->

Choosing a delivery mode is only half the problem. The harder question is: **what does your application do when delivery is ambiguous?** This chapter presents six strategies for handling RPC failures, drawn from FoundationDB's production experience (`fdbrpc.h`, `NativeAPI.actor.cpp`, `ClusterController.actor.cpp`). Simulation testing with chaos injection is what proves you picked the right strategy.

## The Core Insight

Most RPC frameworks treat network failures as exceptions. The connection drops, you get a timeout, and then what? Retry? But the server might have already processed the request. Skip it? But the server might not have received it.

FoundationDB's answer, and moonpool's, is to make this ambiguity **a first-class error**. `MaybeDelivered` tells you exactly what happened: the connection failed, and you do not know whether the request was processed. The application must decide what to do, and simulation testing will verify that decision under thousands of failure scenarios.

## The Six Strategies

### Strategy 1: Idempotent by Design

The simplest and most powerful approach. Design your request to describe the **desired end state**, not a delta. Re-delivery is harmless because applying the same state twice produces the same result.

```rust
// BAD: delta-based — duplicate delivery doubles the effect
TransferRequest { from: "A", to: "B", amount: 100 }

// GOOD: state-based — duplicate delivery is a no-op
SetBalanceRequest { account: "A", balance: 900, version: 42 }
```

Examples: worker registration ("I am node X with capabilities Y"), configuration updates ("set parameter X to value Y"), membership heartbeats.

Use `get_reply` freely with this strategy. The server can safely process duplicates. This is the default choice when you can reformulate the operation as a state assertion.

### Strategy 2: Generation Numbers

Tag each request with a monotonic sequence number. The server tracks the last-seen number per client and ignores old duplicates:

```rust
struct RegisterRequest {
    node_id: NodeId,
    generation: u64,  // monotonically increasing per client
    capabilities: Vec<Capability>,
}

// Server side:
if req.generation <= last_seen_generation[&req.node_id] {
    return Ok(stale_response);  // already processed
}
last_seen_generation.insert(req.node_id, req.generation);
```

Use `get_reply` with this strategy. The reliable transport retransmits, and the server deduplicates via the generation check.

### Strategy 3: Fire-and-Forget

Use `send` for messages where losing one is tolerable. The next message compensates: heartbeats, advisory notifications, metric reports.

The key test: if you send the message twice, is that worse than sending it zero times? If neither matters much, fire-and-forget is the right choice.

### Strategy 4: Read-Before-Retry

On `MaybeDelivered`, read the server's state to determine whether the previous request succeeded before deciding to retry:

```rust
match delivery::try_get_reply(&transport, &ep, commit_req, codec).await {
    Ok(response) => Ok(response),
    Err(ReplyError::MaybeDelivered) => {
        // Query the server: did my commit go through?
        let status = check_commit_status(&transport, &ep, commit_id).await?;
        match status {
            CommitStatus::Committed(version) => Ok(committed(version)),
            CommitStatus::NotFound => {
                // Safe to retry — the original was never processed
                delivery::try_get_reply(&transport, &ep, commit_req, codec).await
            }
        }
    }
    Err(e) => Err(e),
}
```

This is FoundationDB's approach for client commits (`NativeAPI.actor.cpp:6829-6866`). It requires the server to support a status query, but it gives you exactly-once semantics without requiring true distributed transactions.

Use `try_get_reply` with this strategy. The at-most-once guarantee means you know the server processed it at most once, and the read-before-retry resolves the ambiguity.

### Strategy 5: Well-Known Endpoint Retry

For endpoints that survive process restarts (coordinators, cluster controllers), catch `BrokenPromise` and retry with backoff:

```rust
loop {
    match delivery::get_reply(&transport, &coordinator_ep, req.clone(), codec) {
        Ok(future) => match future.await {
            Ok(response) => return Ok(response),
            Err(ReplyError::BrokenPromise) => {
                // Coordinator restarted — same endpoint, retry
                time.sleep(jittered_delay).await;
                continue;
            }
            Err(e) => return Err(e),
        },
        Err(e) => return Err(e.into()),
    }
}
```

This only works for well-known tokens that are registered at the same endpoint across restarts. Ephemeral endpoints (dynamically allocated UIDs) cannot use this pattern because the new process instance has different endpoints.

### Strategy 6: AtMostOnce Flag

When multiple equivalent servers can handle the same request, the question becomes whether to retry on an alternative after failure:

- **Idempotent requests** (reads): retry freely on the next server
- **Non-idempotent requests** (commits): propagate `MaybeDelivered` to the caller

This is FoundationDB's load balancer pattern. Moonpool does not yet have a built-in load balancer, but the pattern applies whenever you maintain a list of alternative endpoints for the same logical service.

## The Decision Flowchart

```text
Can you lose the message entirely?
  YES --> Strategy 3: send (fire-and-forget)
  NO
   |
Can you reformulate as "set state = X"?
  YES --> Strategy 1: idempotent-by-design + get_reply
  NO
   |
Can the server track per-client sequence numbers?
  YES --> Strategy 2: generation dedup + get_reply
  NO
   |
Can you read the state after failure to check?
  YES --> Strategy 4: try_get_reply + read-before-retry
  NO
   |
Is the endpoint well-known and survives restarts?
  YES --> Strategy 5: retry on BrokenPromise
  NO  --> Strategy 2 (add server-side tracking)
```

## Simulation Proves Your Strategy

The reason these strategies matter in moonpool is that **simulation testing will find the bugs** if you pick the wrong one.

A process that uses `get_reply` for a non-idempotent request will see duplicate processing when the chaos engine severs and restores connections. A process that uses `try_get_reply` without handling `MaybeDelivered` will silently drop operations when the chaos engine triggers disconnects. A fire-and-forget heartbeat that should have been reliable will cause false failure detection when the chaos engine delays messages.

The simulation does not know which strategy is "correct" for your use case. But it generates the failure patterns that expose incorrect choices. Run with `UntilAllSometimesReached(1000)` and let the chaos engine prove that your RPC strategy handles every failure mode your system will encounter in production.
