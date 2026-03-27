# Transport Simulation Workload Suite

## Context

The previous transport simulation suite (4,311 lines) was deleted in commit `bc68aba` because of fundamental design flaws:
- Structurally impossible assertions (e.g., asserting retransmission in a receiver)
- `FixedCount` iteration gambling instead of coverage-driven exploration
- Coverage gaps papered over by removing assertions
- TransportTimelineCheck attached to wrong scenarios

This plan designs a new simulation from scratch to **find real bugs** in the transport layer (Peer, NetTransport, FailureMonitor, RPC delivery modes) using proper Process/Workload separation, moonpool-explorer from day 1, and conservative verified assertions.

The discover-properties analysis already found a **likely real bug**: reliable message requeue on write failure silently drops the message if the queue is full (`core.rs:1104`), violating the at-least-once contract.

---

## Architecture Overview

**Single unified simulation** — one binary `sim-transport` in a dedicated crate `moonpool-transport-sim/`.

- **Process**: `EchoServerProcess` — accepts connections, runs multi-method echo service (hand-rolled endpoints, no macro)
- **Workload**: `TransportClientWorkload` — drives random operations across all 4 delivery modes
- **Invariants**: Per-mode timeline invariants + delivery contract checker
- **Topology**: Random per seed (1-5 servers, 1-10 clients)
- **Chaos**: Network-only (no attrition), randomized duration per seed
- **Exploration**: moonpool-explorer with adaptive forking from day 1

---

## TODO Steps

### Step 1: Create crate `moonpool-transport-sim/`

- [ ] Create `moonpool-transport-sim/Cargo.toml`
- [ ] Create `moonpool-transport-sim/src/lib.rs`
- [ ] Create `moonpool-transport-sim/src/bin/sim/transport.rs` (binary entry point)
- [ ] Add `"moonpool-transport-sim"` to workspace `Cargo.toml` members
- [ ] Add `sim-transport` entry to `xtask/src/main.rs` SIM_BINARIES with `sancov_crates: "moonpool_transport"`

### Step 2: Define the echo service (hand-rolled endpoints)

- [ ] Create `src/service.rs`

**3 methods** with hand-rolled UIDs (no `#[service]` macro):

| Method | UID | Behavior |
|--------|-----|----------|
| `echo` | `UID(0xECH0_0001, 1)` | Echo request back unchanged |
| `echo_delayed` | `UID(0xECH0_0001, 2)` | Echo after random simulated delay |
| `echo_or_fail` | `UID(0xECH0_0001, 3)` | Echo normally; `buggify!()` drops the ReplyPromise (BrokenPromise) |

**Request/Response types:**
```rust
#[derive(Serialize, Deserialize, Clone, Debug)]
struct EchoRequest {
    seq_id: u64,
    client_id: String,
    mode: DeliveryMode, // tag for invariant tracking
    payload: Vec<u8>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct EchoResponse {
    seq_id: u64,
    client_id: String,
    server_ip: String,
}

enum DeliveryMode { FireAndForget, AtMostOnce, AtLeastOnce, Timeout }
```

Server-side: register 3 endpoints on NetTransport, spawn background tasks per method using `serve()` pattern.

### Step 3: Implement `EchoServerProcess`

- [ ] Create `src/process.rs`

Key behaviors:
- Listens on `ctx.my_ip()`
- Creates `NetTransport` with providers from ctx
- Registers echo/echo_delayed/echo_or_fail endpoints
- Serves until shutdown token cancelled
- No storage — purely transport-focused

### Step 4: Implement `TransportClientWorkload`

- [ ] Create `src/workload.rs`

**Operation alphabet** (weighted random per call):

| Operation | Weight | Description |
|-----------|--------|-------------|
| `SendFireAndForget` | 15 | `send()` to random server, random method |
| `SendAtMostOnce` | 20 | `try_get_reply()` to random server |
| `SendAtLeastOnce` | 25 | `get_reply()` to random server |
| `SendWithTimeout` | 15 | `get_reply_unless_failed_for(random 100ms-10s)` |
| `SendToWrongEndpoint` | 5 | Send to non-existent UID, expect NotFound |
| `SmallDelay` | 10 | Sleep 1-50ms |
| `CheckMetrics` | 10 | Record transport stats |

**Timeline events** — emit to per-mode timelines (`"fire_and_forget"`, `"at_most_once"`, `"at_least_once"`, `"timeout"`):
```rust
enum DeliveryEvent {
    Sent { seq_id: u64, server: IpAddr, method: u8 },
    Replied { seq_id: u64, response: EchoResponse },
    Failed { seq_id: u64, error: String },
    TimedOut { seq_id: u64, duration_ms: u64 },
    MaybeDelivered { seq_id: u64 },
}
```

**Drain-aware shutdown:** On `ctx.shutdown()`, stop sending new requests, wait for in-flight `get_reply()` with 2s timeout.

### Step 5: Implement delivery contract invariants

- [ ] Create `src/invariants.rs`

**4 invariants**, one per delivery mode:

1. **FireAndForgetInvariant**:
   - `assert_always!`: No responses expected
   - `assert_sometimes!`: Some fire-and-forget calls complete without error

2. **AtMostOnceInvariant**:
   - `assert_always!`: Each seq_id has at most one resolution (Replied XOR Failed XOR MaybeDelivered)
   - `assert_sometimes!`: MaybeDelivered path exercised
   - `assert_always!`: No phantom replies

3. **AtLeastOnceInvariant**:
   - `assert_always!`: Every seq_id eventually gets Replied (or still in-flight at test end)
   - `assert_always!`: No phantom replies
   - `assert_sometimes!`: Duplicate server-side processing occurs (retransmit after reconnect)

4. **TimeoutInvariant**:
   - `assert_always!`: TimedOut only after sustained failure duration elapsed
   - `assert_always!`: No response after timeout declared
   - `assert_sometimes!`: Timeout path actually exercised

**Cross-mode invariant** (uses `sim:faults` timeline):
- `assert_sometimes!`: Faults injected AND messages still delivered (system recovers)
- `assert_sometimes!`: Connection drop during RPC (fault/delivery interleaving)

### Step 6: Transport-internal assertions (from discover-properties)

- [ ] Add assertions to `moonpool-transport/src/peer/core.rs` (~6 assertions):
  1. Connection state sync: `assert_always!(connection.is_some() == metrics.is_connected)`
  2. Unreliable queue cleared: `assert_always!(unreliable_queue.is_empty())` after clear on connection loss
  3. Ping timeout count reset: `assert_always!(timeout_count == 0)` after SendPing transition
  4. Backoff monotonicity: `assert_always!(next_delay >= current_delay || next_delay == max)`
  5. Reliable queue preserves failed message: `assert_always!(reliable_queue.len() > 0)` after requeue
  6. Queue metrics consistency: `assert_always!(metric_queue_size == actual_queue_size)`

- [ ] Add assertions to `moonpool-transport/src/rpc/failure_monitor.rs` (~2):
  7. Eviction only on capacity: `assert_always!(len >= max)` before eviction
  8. Failure monitor matches connection: `assert_always!(fm_status == Available when connected)`

- [ ] Add assertion to `moonpool-transport/src/rpc/net_transport.rs` (~1):
  9. Pending replies closed on disconnect: `assert_sometimes!(entries not empty on disconnect)`

- [ ] Add assertion to `moonpool-transport/src/rpc/delivery.rs` (~1):
  10. Sustained failure timeout enforced: `assert_always!(elapsed >= duration)` before returning MaybeDelivered

### Step 7: Custom transport report

- [ ] Create `src/report.rs`

```rust
struct TransportSimReport {
    total_messages_sent: u64,
    total_messages_received: u64,
    per_mode_breakdown: HashMap<DeliveryMode, ModeStats>,
    reconnection_count: u64,
    broken_promises_count: u64,
    endpoint_not_found_count: u64,
    duplicate_server_processing: u64,
}
```

### Step 8: Binary entry point + xtask wiring

- [ ] Wire up `src/bin/sim/transport.rs` with SimulationBuilder
- [ ] Add `sim-transport` to xtask SIM_BINARIES

### Step 9: Verify

- [ ] `nix develop --command cargo build --bin sim-transport`
- [ ] `nix develop --command cargo clippy -- -D warnings`
- [ ] `nix develop --command cargo fmt`
- [ ] `cargo xtask sim run transport`
- [ ] Validate all `assert_sometimes!` fire (pass_count > 0)
- [ ] Validate no deadlocks (100% success rate)
- [ ] Check if requeue bug is caught by `reliable_queue_preserves_failed_message`

---

## File Summary

### New files (moonpool-transport-sim/):
| File | Purpose | ~LOC |
|------|---------|------|
| `Cargo.toml` | Crate manifest | 25 |
| `src/lib.rs` | Module declarations + re-exports | 20 |
| `src/service.rs` | Echo service types + hand-rolled endpoints | 150 |
| `src/process.rs` | EchoServerProcess impl | 120 |
| `src/workload.rs` | TransportClientWorkload + operations | 300 |
| `src/invariants.rs` | 4 delivery mode invariants | 200 |
| `src/report.rs` | Custom transport metrics | 80 |
| `src/bin/sim/transport.rs` | Binary entry point | 50 |

### Modified files:
| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | Add member |
| `xtask/src/main.rs` | Add sim binary entry |
| `moonpool-transport/src/peer/core.rs` | ~6 assertions |
| `moonpool-transport/src/rpc/failure_monitor.rs` | ~2 assertions |
| `moonpool-transport/src/rpc/net_transport.rs` | ~1 assertion |
| `moonpool-transport/src/rpc/delivery.rs` | ~1 assertion |

**Estimated total: ~950 LOC new + ~30 LOC modified**
