# Fault Reference

<!-- toc -->

Consolidated quick-reference of every fault moonpool-sim can inject, organized by category. For detailed explanations and examples, see [Network Faults](../part3-building/network-faults.md), [Storage Faults](../part3-building/storage-faults.md), and [Attrition: Process Reboots](../part3-building/attrition.md).

All defaults below refer to the values in `ChaosConfiguration::default()` and `StorageConfiguration::default()`. When using `random_for_seed()`, these values are randomized per seed within documented ranges.

## Network Faults

Configured via `ChaosConfiguration` (nested under `NetworkConfiguration::chaos`).

### Connection Failures

| Fault | Config Field | Default | Real-World Scenario |
|-------|-------------|---------|---------------------|
| Random connection close | `random_close_probability` | 0.001% | Reconnection logic, message redelivery, connection pooling |
| Asymmetric close | `random_close_explicit_ratio` | 30% explicit (FIN), 70% silent (RST) | Half-closed sockets, FIN vs RST handling |
| Close cooldown | `random_close_cooldown` | 5s | Prevents cascading failures after a close event |
| Connect failure | `connect_failure_mode` | `Probabilistic` (50% refused, 50% hang) | Connection establishment retries, timeout handling |
| Connect failure probability | `connect_failure_probability` | 50% | Ratio of failed vs hanging connections |

### Latency and Congestion

| Fault | Config Field | Default | Real-World Scenario |
|-------|-------------|---------|---------------------|
| Latency distribution | `latency_distribution` | `Uniform` | P99/P99.9 tail latency testing |
| Slow latency spike | `slow_latency_probability` | 0.1% (bimodal mode only) | GC pauses, cross-datacenter hops |
| Slow latency multiplier | `slow_latency_multiplier` | 10x normal | Magnitude of tail latency spikes |
| Write clogging | `clog_probability` / `clog_duration` | 0%, 100-300ms | Backpressure handling, flow control |
| Clock drift | `clock_drift_enabled` / `clock_drift_max` | enabled, 100ms | Lease expiration, distributed consensus, TTL handling |
| Buggified delay | `buggified_delay_probability` / `buggified_delay_max` | 25%, 100ms | Race conditions, timing-dependent bugs |
| Handshake delay | `handshake_delay_enabled` / `handshake_delay_max` | enabled, 10ms | TLS negotiation, connection startup overhead |

### Network Partitions

| Fault | Config Field | Default | Real-World Scenario |
|-------|-------------|---------|---------------------|
| Random partition | `partition_probability` | 0% | Split-brain, quorum loss, leader election |
| Partition duration | `partition_duration` | 200ms-2s | Recovery time after network heal |
| Partition strategy | `partition_strategy` | `Random` | `Random` / `UniformSize` / `IsolateSingle` patterns |

Manual partition methods are also available on `SimWorld`: `partition_pair()`, `partition_send_from()`, `partition_recv_to()`.

### Data Integrity

| Fault | Config Field | Default | Real-World Scenario |
|-------|-------------|---------|---------------------|
| Bit flips | `bit_flip_probability` | 0.01% | CRC/checksum validation, data corruption detection |
| Flip range | `bit_flip_min_bits` / `bit_flip_max_bits` | 1-32 bits | Power-law distribution of corruption severity |
| Flip cooldown | `bit_flip_cooldown` | 0 (no cooldown) | Rate-limiting corruption events |
| Partial writes | `partial_write_max_bytes` | 1000 bytes | TCP fragmentation, message framing |

### Half-Open Connections

| Fault | Method | Real-World Scenario |
|-------|--------|---------------------|
| Peer crash simulation | `simulate_peer_crash()` | TCP keepalive, heartbeat detection, silent failures |
| Half-open error detection | `should_half_open_error()` | Timeout-based failure detection |
| Stable connection exemption | `mark_connection_stable()` | Exempt supervision channels from chaos |

## Storage Faults

Configured via `StorageConfiguration`. All fault probabilities default to 0% and must be enabled explicitly or via `random_for_seed()`.

| Fault | Config Field | Default | Real-World Scenario |
|-------|-------------|---------|---------------------|
| Read corruption | `read_fault_probability` | 0% | ECC failures, DRAM bit flips, media degradation |
| Write corruption | `write_fault_probability` | 0% | Bad sectors, controller bugs, disk full |
| Crash fault (torn writes) | `crash_fault_probability` | 0% | Power loss mid-I/O, crash consistency |
| Misdirected write | `misdirect_write_probability` | 0% | Firmware bugs, wrong block written |
| Misdirected read | `misdirect_read_probability` | 0% | Controller errors, wrong block read |
| Phantom write | `phantom_write_probability` | 0% | Drive lies about durability |
| Sync failure | `sync_failure_probability` | 0% | fsync fails, disk full |

### Storage Performance Simulation

Storage also simulates realistic performance characteristics independent of fault injection.

| Parameter | Config Field | Default | Description |
|-----------|-------------|---------|-------------|
| IOPS | `iops` | 25,000 | I/O operations per second limit |
| Bandwidth | `bandwidth` | 150 MB/s | Maximum throughput |
| Read latency | `read_latency` | 50-200us | Per-read operation delay |
| Write latency | `write_latency` | 100-500us | Per-write operation delay |
| Sync latency | `sync_latency` | 1-5ms | Per-sync/flush delay |

## Process Lifecycle Faults

Configured via [`Attrition`](../part3-building/attrition.md) (built-in) or custom [`FaultInjector`](../part3-building/chaos.md) implementations.

| Fault | Mechanism | Behavior |
|-------|-----------|----------|
| Graceful reboot | `RebootKind::Graceful` | Signal shutdown token, wait grace period (default 2-5s), force kill, restart after recovery delay (default 1-10s) |
| Crash reboot | `RebootKind::Crash` | Immediate task abort, all connections reset, restart after recovery delay |
| Crash + wipe | `RebootKind::CrashAndWipe` | Crash behavior + delete all persistent storage (total data loss) |
| Continuous attrition | `Attrition` config | Random reboots during chaos phase with weighted `prob_graceful`/`prob_crash`/`prob_wipe` and `max_dead` limit |

## Configuration Presets

| Preset | Description |
|--------|-------------|
| `NetworkConfiguration::random_for_seed()` | All chaos parameters randomized per seed for comprehensive testing |
| `NetworkConfiguration::fast_local()` | 1-10us latencies, all chaos disabled |
| `ChaosConfiguration::disabled()` | Zero probability for every fault category |
| `StorageConfiguration::random_for_seed()` | Randomized faults (0.001%-0.1%), varied IOPS (10K-100K), varied bandwidth (50-500 MB/s) |
| `StorageConfiguration::fast_local()` | 1M IOPS, 1 GB/s bandwidth, 1us latencies, all faults disabled |

See [Configuration Reference](./03-configuration.md) for the complete builder API and all configuration types.
