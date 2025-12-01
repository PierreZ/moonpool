# Fault Injection Guide

Moonpool implements deterministic fault injection following FoundationDB's
simulation testing patterns. This document describes all chaos mechanisms,
their trigger conditions, and what each tests.

## Philosophy

FoundationDB's insight: **bugs hide in error paths**. Production code rarely
exercises timeout handlers, retry logic, or failure recovery. Deterministic
simulation with fault injection finds these bugs before production does.

Key principles:
- **Deterministic**: Same seed produces same faults, enabling reproducible debugging
- **Comprehensive**: Test all failure modes: network, timing, corruption
- **Low probability**: Faults are rare enough to allow progress, frequent enough to catch bugs

## Core Buggify System

The buggify system provides location-based fault injection. Each code location
is randomly **activated** once per simulation run, then fires probabilistically.

```rust
// 25% probability when activated
if buggify!() {
    return Err(SimulatedFailure);
}

// Custom probability
if buggify_with_prob!(0.02) {
    corrupt_data();
}
```

### Two-Phase Activation

1. **Activation** (once per location per seed): `random() < activation_prob`
2. **Firing** (each call): If active, `random() < firing_prob`

This ensures consistent behavior within a simulation run while varying
which locations are active across different seeds.

| Parameter | Default | FDB Reference |
|-----------|---------|---------------|
| `activation_prob` | 25% | `Buggify.h:79-88` |
| `firing_prob` | 25% | `P_GENERAL_BUGGIFIED_SECTION_FIRES` |

## Fault Injection Mechanisms

### 1. Random Connection Close

Simulates random TCP connection failures during I/O operations.

| Setting | Default | FDB Reference |
|---------|---------|---------------|
| `random_close_probability` | 0.001% | `sim2.actor.cpp:584` |
| `random_close_cooldown` | 5s | `sim2.actor.cpp:583` |
| `random_close_explicit_ratio` | 30% | `sim2.actor.cpp:602` |

**Trigger conditions:**
1. `buggify_with_prob!(random_close_probability)` fires
2. Cooldown elapsed since last random close

**Failure modes:**
- **Explicit (30%)**: Returns `ConnectionReset` error immediately
- **Silent (70%)**: Marks connection closed, returns EOF on reads

**Direction logic (FDB pattern):**
```rust
let a = random();
let close_recv = a < 0.66;  // ~66% close recv side
let close_send = a > 0.33;  // ~66% close send side
// 0.33 < a < 0.66 = both directions
```

**Tests:** Connection failure handling, reconnection logic, message redelivery

### 2. Bit Flip Corruption

Simulates network packet corruption with power-law bit count distribution.

| Setting | Default | FDB Reference |
|---------|---------|---------------|
| `bit_flip_probability` | 0.01% | `FlowTransport.actor.cpp:1297` |
| `bit_flip_min_bits` | 1 | - |
| `bit_flip_max_bits` | 32 | - |
| `bit_flip_cooldown` | 0 | - |

**Trigger conditions:**
1. Cooldown elapsed
2. `buggify_with_prob!(bit_flip_probability)` fires

**Power-law distribution:**
```rust
// Biased toward fewer bits: 1-2 common, 32 rare
bit_count = 1 + leading_zeros(random_u64())
```

**Tests:** CRC32C checksum validation, graceful corruption handling

### 3. Connection Establishment Failure

Simulates TCP connect failures and hanging connections.

| Setting | Default | FDB Reference |
|---------|---------|---------------|
| `connect_failure_mode` | `Probabilistic` | `sim2.actor.cpp:1243-1250` |
| `connect_failure_probability` | 50% | - |

**Failure modes (`ConnectFailureMode` enum):**
- **`Disabled`**: No connection failures injected
- **`AlwaysFail`**: Always fail with `ConnectionRefused` when buggified
- **`Probabilistic`**:
  - 50%: Fail with `ConnectionRefused`
  - 50%: Hang forever (`pending()`)

**Tests:** Connection timeout handling, retry logic, dial backoff

### 4. Clock Drift

Simulates real-world clock drift between `now()` and `timer()`.

| Setting | Default | FDB Reference |
|---------|---------|---------------|
| `clock_drift_enabled` | true | `sim2.actor.cpp:1058-1064` |
| `clock_drift_max` | 100ms | - |

**FDB formula:**
```rust
// timer progressively catches up to max
let gap = (max_timer - timer_time).as_secs_f64();
let delta = random() * gap / 2.0;
timer_time += delta;
```

**Guarantees:**
- `timer()` >= `now()` always
- `timer()` <= `now() + clock_drift_max`
- Monotonically increasing

**Tests:** Timeout handling, lease expiration, heartbeat detection, leader election

### 5. Buggified Delays

Adds random delays to sleep operations using power-law distribution.

| Setting | Default | FDB Reference |
|---------|---------|---------------|
| `buggified_delay_enabled` | true | `sim2.actor.cpp:1100-1105` |
| `buggified_delay_max` | 100ms | `MAX_BUGGIFIED_DELAY` |
| `buggified_delay_probability` | 25% | - |

**FDB formula:**
```rust
// Most delays near 0, occasional full delay
extra = max_delay * random().powf(1000.0)
```

**Tests:** Timing assumptions, race conditions, operation ordering

### 6. Partial/Short Writes

Simulates TCP backpressure causing partial writes.

| Setting | Default | FDB Reference |
|---------|---------|---------------|
| `partial_write_max_bytes` | 1000 | `FlowTransport.actor.cpp` |

**Trigger:** `buggify!()` on non-empty writes

**Behavior:** Truncates write to random `0..max_bytes`, requeues remainder

**Tests:** Message fragmentation, reassembly, write loops

### 7. Write Clogging

Simulates temporary network congestion.

| Setting | Default | FDB Reference |
|---------|---------|---------------|
| `clog_probability` | 0% (disabled) | `Net2.actor.cpp` |
| `clog_duration` | 100-300ms | - |

**Trigger:** `random() < clog_probability` on write

**Behavior:** Write returns `Poll::Pending` for duration

**Tests:** Async backpressure, buffer management

### 8. Network Partitions

Simulates network partition events with directional control.

| Setting | Default | FDB Reference |
|---------|---------|---------------|
| `partition_probability` | 0% (disabled) | `sim2.actor.cpp:1051+` |
| `partition_duration` | 200ms-2s | - |

**Partition types:**
- `partition_pair(from, to, duration)`: Directional A→B block
- `partition_send_from(ip, duration)`: All outgoing blocked
- `partition_recv_to(ip, duration)`: All incoming blocked

**Tests:** Split-brain handling, reconnection, consensus under partition

### 9. Peer Write Failures

Forces write failures in the Peer layer for message requeuing tests.

| Setting | Default | FDB Reference |
|---------|---------|---------------|
| (inline) | 2% | `discardUnreliablePackets()` |

**Trigger:** `buggify_with_prob!(0.02)` in peer write path

**Tests:** Reliable delivery, message requeuing, at-least-once semantics

### 10. TCP Operation Latencies

Simulates realistic network timing for all TCP operations.

| Operation | Default Range | `random_for_seed()` Range |
|-----------|---------------|---------------------------|
| `bind_latency` | 50-150µs | 10-300µs |
| `accept_latency` | 1-6ms | 1-15ms |
| `connect_latency` | 1-11ms | 1-100ms |
| `read_latency` | 10-60µs | 5-200µs |
| `write_latency` | 50-600µs | 50-2000µs |

**Configuration:**
```rust
use moonpool_foundation::NetworkConfiguration;

// Realistic latencies (default)
let config = NetworkConfiguration::default();

// Randomized per seed for chaos testing
let config = NetworkConfiguration::random_for_seed();

// Minimal latencies for fast tests
let config = NetworkConfiguration::fast_local();  // 1-10µs
```

**Mechanism:** Each operation samples uniformly from its range using `sample_duration()`.

**Tests:** Timeout handling, operation ordering, async scheduling

### 11. Message Delivery Scheduling

Controls timing of data transfer between connection pairs.

**Delay strategy:**
- **More messages queued**: 1ns delay (process immediately)
- **Last message in buffer**: Full `write_latency` delay

**Per-connection ordering:**
- Tracks `next_send_time` per connection
- Ensures FIFO delivery within each connection
- Delays accumulate for back-to-back sends

**Implementation:** `ProcessSendBuffer` → schedules `DataDelivery` event

**Tests:** Message ordering, throughput under load, pipeline stalls

## Configuration

All chaos settings live in `ChaosConfiguration`:

```rust
use moonpool_foundation::ChaosConfiguration;

// Full chaos (default)
let chaos = ChaosConfiguration::default();

// No chaos (fast tests)
let chaos = ChaosConfiguration::disabled();

// Randomized per seed
let chaos = ChaosConfiguration::random_for_seed();
```

## Usage for Workload Authors

### Strategic Placement

Place `buggify!()` calls at:
- Error handling paths
- Timeout boundaries
- Retry logic entry points
- Resource limit checks
- State transitions

### Debugging Failing Seeds

```rust
SimulationBuilder::new()
    .set_seed(failing_seed)  // Reproduce exact failure
    .run_count(FixedCount(1))
    // Enable RUST_LOG=error for visibility
```

### Coverage Goals

Use `sometimes_assert!` to verify error paths execute:

```rust
if buggify!() {
    sometimes_assert!("timeout_triggered");
    return Err(Timeout);
}
```

Multi-seed testing with `UntilAllSometimesReached(1000)` ensures all
`sometimes_assert!` statements fire across the seed space.
