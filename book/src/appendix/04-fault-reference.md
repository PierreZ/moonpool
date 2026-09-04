# Fault Reference

<!-- toc -->

Consolidated quick-reference of every fault moonpool-sim can inject, organized
by category. For detailed explanations and examples, see
[Network Faults](../part3-building/10-network-faults.md),
[Storage Faults](../part3-building/11-storage-faults.md), and
[Attrition: Process Reboots](../part3-building/09-attrition.md).

Every fault listed below is automatically emitted to the `"sim_fault"`
[event timeline](../part3-building/17-events-and-invariants.md) as a
`SimFaultEvent`. Invariants can read these to correlate application behavior
with infrastructure faults.

All defaults below refer to the values in `ChaosConfiguration::default()` and `StorageConfiguration::default()`. When using `random_for_seed()`, these values are randomized per seed within documented ranges.

## Network Faults

Configured via `ChaosConfiguration` (nested under `NetworkConfiguration::chaos`).

### Connection Failures

| Fault | Config Field | Default | Real-World Scenario |
|-------|-------------|---------|---------------------|
| Random connection close | `random_close_probability` | 0.001% | Reconnection logic, message redelivery, connection pooling |
| Close error surfacing | `random_close_explicit_ratio` | 30% immediate error, 70% silent close | Explicit-error and timeout-based detection |
| Close cooldown | `random_close_cooldown` | 5s | Prevents cascading failures after a close event |
| Connect failure | `connect_failure_mode` | `Probabilistic` (50% refused, 50% hang) | Connection establishment retries, timeout handling |
| Connect failure probability | `connect_failure_probability` | 50% | Ratio of failed vs hanging connections |
| Stable connection exemption | `mark_connection_stable()` | Manual | Exempt supervision channels from random-close chaos |

### Latency and Congestion

| Fault | Config Field | Default | Real-World Scenario |
|-------|-------------|---------|---------------------|
| Operation latency shape | `bind/accept/connect/write_latency` | `Uniform` | P99/P99.9 tail latency testing |
| Exponential tail | `LatencyDistribution::Exponential { min, mean }` | opt-in | Slow disks, GC pauses (TigerBeetle model) |
| Bimodal tail | `LatencyDistribution::Bimodal { fast_range, slow_range, slow_probability }` | opt-in | Rare cross-datacenter hops, GC spikes (FoundationDB model) |
| Write clogging | `clog_probability` / `clog_duration` | 0%, 100-300ms | Backpressure handling, flow control |
| Per-pair permanent latency | `max_pair_latency` | `ZERO..ZERO` (off) | One stably-slow peer blocking quorum, asymmetric link delay |
| Distance-based link latency | `link_latency` (`LinkLatencyConfig`) | `None` (off) | Loopback vs rack vs region hops, cross-datacenter replication cost |
| Clock drift | `clock_drift_enabled` / `clock_drift_max` | enabled, 100ms | Lease expiration, distributed consensus, TTL handling |
| Buggified delay | `buggified_delay_probability` / `buggified_delay_max` | 25%, 100ms | Race conditions, timing-dependent bugs |
| Handshake delay | `handshake_delay_enabled` / `handshake_delay_max` | enabled, 10ms | TLS negotiation, connection startup overhead |

### Network Partitions

| Fault | Config Field | Default | Real-World Scenario |
|-------|-------------|---------|---------------------|
| Random partition | `partition_probability` | 0% | Split-brain, quorum loss, leader election |
| Partition duration | `partition_duration` | 200ms-2s | Recovery time after network heal |
| Partition strategy | `partition_strategy` | `Random` | `Random` / `UniformSize` / `IsolateSingle` patterns |
| Failure-domain partition | `partition_strategy = IsolateZone` / `IsolateDatacenter` | `Random` | Rack or region cut (needs a `cluster` topology, else falls back to `Random`) |
| One-way partition | `partition_strategy = AsymmetricSend` / `AsymmetricRecv` | `Random` | Half-reachable node, failure detectors that infer liveness from the wrong direction |

Manual partition methods are also available on `SimWorld`: `partition_pair()`, `partition_send_from()`, and `partition_recv_to()`. `partition_pair(from, to, ...)` creates one directed pair entry; `restore_partition(from, to)` removes pair entries in both directions so a bidirectional cut can be healed with one call. Send-wide and receive-wide partitions are restored by their own expiry events.

### Data Integrity

| Fault | Config Field | Default | Real-World Scenario |
|-------|-------------|---------|---------------------|
| Bit flips | `bit_flip_probability` | 0.01% | CRC/checksum validation, data corruption detection |
| Flip range | `bit_flip_min_bits` / `bit_flip_max_bits` | 1-32 bits | Power-law distribution of corruption severity |
| Flip cooldown | `bit_flip_cooldown` | 0 (no cooldown) | Rate-limiting corruption events |
| Partial writes | `partial_write_max_bytes` | 1000 bytes | TCP fragmentation, message framing |
| Partial reads | `partial_read_max_bytes` | 1000 bytes | TCP short reads, message reassembly / framing |

## Storage Faults

Configured via `StorageConfiguration`. All fault probabilities default to 0%
and must be enabled explicitly or via `random_for_seed()`. Storage faults are
scoped per process. `StorageEngine` owns the default profile, per-process
overrides, and degradation episodes, then resolves the profile from each
persistent file's owner. Use
`SimWorld::set_process_storage_config(ip, config)` to assign different fault
profiles to individual processes.

Each delayed storage completion targets one exact `OperationId`. Crash, wipe,
and shutdown cancel live schedules, store explicit errors for interrupted
operations, and wake their registered callers. Missing completion state is
never interpreted as success.

| Fault | Config Field | Default | Real-World Scenario |
|-------|-------------|---------|---------------------|
| Read corruption | `read_fault_probability` | 0% | ECC failures, DRAM bit flips, media degradation |
| Write corruption | `write_fault_probability` | 0% | Bad sectors, controller bugs, disk full |
| Crash fault (torn writes) | `crash_fault_probability` | 0% | Power loss mid-I/O, crash consistency |
| Misdirected write | `misdirect_write_probability` | 0% | Firmware bugs, wrong block written |
| Misdirected read | `misdirect_read_probability` | 0% | Controller errors, wrong block read |
| Phantom write | `phantom_write_probability` | 0% | Drive lies about durability |
| Sync failure | `sync_failure_probability` | 0% | fsync fails, disk full |

### Per-Process Storage Operations

| Method | Parameters | Description |
|--------|-----------|-------------|
| `SimWorld::set_process_storage_config(ip, config)` | `IpAddr`, `StorageConfiguration` | Set per-process fault config (overrides global) |
| `SimWorld::simulate_crash_for_process(ip, close_files)` | `IpAddr`, `bool` | Simulate power loss: torn writes, optional file close |
| `SimWorld::wipe_storage_for_process(ip)` | `IpAddr` | Delete all storage owned by the process |
| `SimWorld::storage_provider(ip)` | `IpAddr` | Create a `SimStorageProvider` scoped to this process |

### Storage Performance Simulation

Storage also simulates realistic performance characteristics independent of fault injection.

| Parameter | Config Field | Default | Description |
|-----------|-------------|---------|-------------|
| IOPS | `iops` | 25,000 | I/O operations per second limit |
| Bandwidth | `bandwidth` | 150 MB/s | Maximum throughput |
| Read latency | `read_latency` | `Uniform` 50-200us | Per-read operation delay (a `LatencyDistribution`) |
| Write latency | `write_latency` | `Uniform` 100-500us | Per-write operation delay (a `LatencyDistribution`) |
| Sync latency | `sync_latency` | `Uniform` 1-5ms | Per-sync/flush delay (a `LatencyDistribution`) |

### Dynamic Disk Degradation Episodes

Episodic degradation layered on top of steady-state timing (FoundationDB's `DiskFailureInjector`). Off by default and scoped per process (per owning IP): one episode freezes or throttles every file that process owns together, modelling device-level degradation, while other machines stay unaffected. While disabled, the episode state machine never draws from the RNG stream, so steady-state runs stay deterministic.

| Episode | Config Field | Default | While active |
|---------|-------------|---------|--------------|
| Stall | `disk_stall_probability` / `disk_stall_duration` | 0%, 0ms | Disk frozen until expiry; I/O waits out the window |
| Throttle | `disk_throttle_probability` / `disk_throttle_duration` | 0%, 0ms | Effective IOPS/bandwidth divided by the multipliers |
| Throttle factor | `disk_throttle_iops_multiplier` / `disk_throttle_bandwidth_multiplier` | 1.0 | Divisor applied to IOPS / bandwidth during a throttle |

### Disk Failure

A disk that never answers (FoundationDB's `failedDisk`, where `waitUntilDiskReady()` returns `Never()`). Off by default and scoped per process: every read, write, sync, or `set_len` issued to a failed disk is accepted and stays `Pending` for the rest of the run, with no error and no scheduled completion. At most one disk is failed at a time; a crash or wipe of the owning process replaces it and fails the parked operations with `OperationInterrupted`. Recovery mode stops new failures but keeps one already in force. `SimWorld::fail_disk_for_process(ip)` is the scripted form.

| Fault | Config Field | Default | Effect |
|-------|-------------|---------|--------|
| Disk failure | `disk_failure_probability` | 0% | Every later I/O on that process's disk parks forever; only the caller's timeout or a process kill unblocks it |

## Process Lifecycle Faults

Configured via [`Attrition`](../part3-building/09-attrition.md) (built-in) or
custom [`FaultInjector`](../part3-building/07-chaos.md) implementations.

| Fault | Mechanism | Behavior |
|-------|-----------|----------|
| Graceful reboot | `RebootKind::Graceful` | Signal shutdown token, wait grace period (default 2-5s), force kill, restart after recovery delay (default 1-10s) |
| Crash reboot | `RebootKind::Crash` | Immediate task abort at the crash instant, all connections reset, restart after recovery delay |
| Crash + wipe | `RebootKind::CrashAndWipe` | Crash behavior + immediate wipe of all persistent storage owned by the process (scoped by IP) |
| Continuous attrition | `Attrition` config | Random reboots during chaos phase with weighted `prob_graceful`/`prob_crash`/`prob_wipe` and `max_dead` limit |
| Correlated reboot | `AttritionScope::PerMachine` / `PerZone` / `PerDatacenter` | Reboot every process of one failure domain together; only fires when the whole group fits in `max_dead` |

## Configuration Presets

| Preset | Description |
|--------|-------------|
| `NetworkConfiguration::random_for_seed()` | All chaos parameters randomized per seed for comprehensive testing |
| `NetworkConfiguration::fast_local()` | 1-10us latencies, all chaos disabled |
| `ChaosConfiguration::disabled()` | Zero probability for every fault category |
| `StorageConfiguration::random_for_seed()` | Randomized faults (0.001%-0.1%), varied IOPS (10K-100K), varied bandwidth (50-500 MB/s) |
| `StorageConfiguration::fast_local()` | 1M IOPS, 1 GB/s bandwidth, 1us latencies, all faults disabled |

See [Configuration Reference](./03-configuration.md) for the complete builder API and all configuration types.
