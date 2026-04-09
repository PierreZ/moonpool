# Load Balancing and Fan-Out

<!-- toc -->

The delivery modes in the previous chapters all talk to a single peer. Real distributed systems need two patterns the four-mode API does not directly express: **picking one peer out of many** (load balancing) and **talking to many peers in parallel** (fan-out). Moonpool ships both as thin layers over the existing `ServiceEndpoint` API, modelled directly on FoundationDB's `loadBalance()` and the TLog commit fan-out.

## Where the Two Patterns Differ

Both patterns start from a list of equivalent backends — N storage servers behind a key range, N TLogs in a replication set, N coordinators of the same cluster. The difference is what we want to do with that list.

| Pattern | Goal | Network calls per request |
|---------|------|---------------------------|
| Load balance | Pick the best one. Retry on the next if it fails. | 1 (sometimes more on retry) |
| Fan-out | Talk to all of them in parallel. Combine the replies. | N |

A read against a replicated key is a load-balance: any replica can answer, we want the lowest-latency one, and we are happy to retry on a different replica if the first one is slow. A commit against a TLog set is a fan-out: every TLog needs to write the mutation, and we wait for all of them (or a quorum) before declaring the commit durable.

## Load Balancing

`load_balance()` takes a group of equivalent endpoints, sends one request, and retries on a different alternative if the chosen one fails. It is the moonpool analog of `fdbrpc/include/fdbrpc/LoadBalance.actor.h:823-1018` with hedging deferred to a follow-up.

```rust
use moonpool::rpc::{
    Alternatives, AtMostOnce, Distance, LoadBalanceConfig, QueueModel, load_balance,
};

let alts = Alternatives::new(vec![
    (replica_a.read.clone(), Distance::SameDc),
    (replica_b.read.clone(), Distance::SameDc),
    (replica_c.read.clone(), Distance::Remote),
]);
let model = QueueModel::new();
let config = LoadBalanceConfig::default();

let value = load_balance(
    &transport,
    &alts,
    GetValueRequest { key: "user/42".into() },
    AtMostOnce::False, // reads are idempotent
    &model,
    &config,
).await?;
```

Three things make this work.

**`Alternatives` sorts by locality.** Each entry carries a `Distance` tag (`SameMachine`, `SameDc`, `Remote`). The constructor sorts by distance ascending and computes `count_best` — the number of entries at the closest tier. Load balancing prefers the local-DC prefix and only falls back to remote entries when every local one is unavailable.

**`QueueModel` tracks smoothed in-flight count and latency per endpoint.** Internally it is a `HashMap<u64, QueueData>` keyed by the high half of the endpoint UID, exactly matching FDB's `getMeasurement(token.first())` convention. Each entry holds a `Smoother` (an exponential moving average with a one-second e-folding constant) for the outstanding count, plus the most recently observed latency. The selection step picks the alternative with the lowest smoothed outstanding count among the viable candidates.

**`AtMostOnce` makes the idempotency contract explicit.** With `AtMostOnce::False` the load balancer issues `get_reply` (reliable) and treats `MaybeDelivered` as a retry trigger. With `AtMostOnce::True` it issues `try_get_reply` (unreliable) and propagates `MaybeDelivered` immediately, so a side-effecting request is never retried on a different alternative when its outcome is ambiguous. This is the central design lever from FDB's `LoadBalance.actor.h:572-625`: side effects must not silently double up.

The retry loop cycles through alternatives in best-first order, marking each one tried. After a full cycle without success it sleeps for an exponentially-growing backoff (`backoff_start` doubled each cycle up to `backoff_max`) and starts a fresh cycle, up to `max_full_cycles` cycles total before returning the most recent error. The defaults — two cycles, 50ms start, 1s cap, 2× multiplier — match FDB's `FLOW_KNOBS->LOAD_BALANCE_*` shape, and every knob lives on `LoadBalanceConfig` so callers can tune per deployment. Production callers wrap the call in their own retry policy if they want indefinite retry.

### When to use a `QueueModel`

A `QueueModel` is shared across many concurrent `load_balance` calls — typically wrap it in an `Rc` and hand out references. It uses `RefCell` internally so the borrow happens for the duration of one method call only, never across an `await`. A fresh model treats every endpoint as zero-outstanding, so the very first request to an unseen alternative looks maximally attractive — exactly what you want for cold-start fairness.

## Fan-Out: Four Completion Semantics

Fan-out helpers send the same request to every endpoint in a slice and combine the replies. Moonpool ships four variants because real systems need different completion conditions.

```rust
use moonpool::rpc::{
    fan_out_all, fan_out_quorum, fan_out_race, fan_out_all_partial,
};
```

### `fan_out_all` — All Must Succeed

The TLog-style "every peer or nothing" pattern. The request is cloned to each endpoint, every reply is awaited, and the first failure aborts the rest. This mirrors FDB's resolver fan-out (`CommitProxyServer.actor.cpp:1127-1179`), where any single resolver failure aborts the commit.

```rust
let resolutions = fan_out_all(
    &transport,
    &resolver_endpoints,
    ResolveTransactionBatchRequest { /* ... */ },
).await?;
// resolutions[i] is the reply from resolver i, in input order
```

The returned `Vec<Resp>` is in input order, so the caller can correlate replies with senders by index.

### `fan_out_quorum` — K of N

The TLog commit pattern from `TagPartitionedLogSystem.actor.cpp:619-687`. The function waits until `required` peers have replied successfully, then drops the rest. If too many peers fail to ever reach the threshold, it returns `QuorumNotMet` immediately rather than waiting on doomed futures.

```rust
let acks = fan_out_quorum(
    &transport,
    &tlog_endpoints,
    TLogCommitRequest { version, mutations },
    /* required = */ tlog_endpoints.len() - anti_quorum,
).await?;
```

The `Vec<Resp>` returned on success is in **completion order**, not input order — the caller is using a quorum vote, not per-peer correlation. `MaybeDelivered` and `BrokenPromise` errors count as ordinary failed peers; fan-out never retries (every peer was already addressed in parallel), so the `AtMostOnce` flag would be a no-op and is intentionally absent from the signature.

### `fan_out_race` — First Success Wins

Send to all, return the first `Ok`, drop the rest. Useful for hedged reads against equivalent replicas when you do not want the bookkeeping cost of a full `QueueModel`.

```rust
let value = fan_out_race(&transport, &replica_endpoints, GetValueRequest { key }).await?;
```

If every peer errors, the function returns `AllFailed { errors }` with one error per peer.

### `fan_out_all_partial` — Wait for All, Return Per-Peer Results

Sometimes you want every peer's outcome — successes and failures together — without aborting. This variant never short-circuits.

```rust
let outcomes: Vec<Result<HealthCheck, RpcError>> =
    fan_out_all_partial(&transport, &all_peers, HealthCheckRequest).await?;

let healthy = outcomes.iter().filter(|r| r.is_ok()).count();
```

The result vector has exactly `endpoints.len()` entries, in input order. The fan-out itself only fails if the input slice is empty.

## Composing the Two

Real systems use both. A read path looks like `load_balance(reads)` to pick one storage server with retry. A write path looks like `fan_out_quorum(commits, durability_quorum)` to make sure enough TLogs persisted the mutation. The `#[service]` macro generates per-method `ServiceEndpoint` clones, so building an `Alternatives` or a `Vec<ServiceEndpoint>` from a slice of generated clients is a one-liner — no separate routing layer required.

Both patterns avoid spawning tasks. They compose futures via `try_join_all`, `FuturesUnordered`, and `tokio::select!`-style polling on the current task, which keeps moonpool's single-threaded determinism contract intact.
