# Fault Reference

<!-- toc -->

Every fault the simulator can inject, with where it is configured and how it shows up. Rates and defaults are in the [Configuration Reference](./03-configuration.md). Every fault is drawn from the one seeded stream, so it replays exactly.

## Network Faults

Enabled by `Chaos::Network(mode)`, limited by `network_fault_mask`. Each family is one `NetworkFault` bit.

| `NetworkFault` | Bit | What happens | Config fields |
|----------------|-----|--------------|---------------|
| `Clog` | `1 << 0` | Delivery of a write is held for a duration, then resumes. | `clog_probability`, `clog_duration` |
| `Partition` | `1 << 1` | Directed partitions. Sends between the chosen IPs are dropped until healed. | `partition_probability`, `partition_duration`, `partition_strategy` |
| `BitFlip` | `1 << 2` | 1 to 32 bits of in-flight data are flipped. | `bit_flip_probability`, `bit_flip_min_bits`, `bit_flip_max_bits`, `bit_flip_cooldown` |
| `RandomClose` | `1 << 3` | A connection closes during I/O. A share is an explicit close, the rest a reset. | `random_close_probability`, `random_close_cooldown`, `random_close_explicit_ratio` |
| `ConnectFailure` | `1 << 4` | `connect` fails or hangs. | `connect_failure_mode`, `connect_failure_probability` |
| `ClockDrift` | `1 << 5` | Each node's clock drifts up to a bound. | `clock_drift_enabled`, `clock_drift_max` |
| `BuggifiedDelay` | `1 << 6` | Timers fire late by an extra random delay. | `buggified_delay_enabled`, `buggified_delay_max`, `buggified_delay_probability` |
| `PairLatency` | `1 << 7` | One IP pair gets permanent extra latency. | `max_pair_latency` |
| `BlackHole` | `1 << 8` | A connection whose sends vanish silently and forever. | `black_hole_probability`, `black_hole_cooldown` |

Partial reads and writes (`partial_write_max_bytes`, `partial_read_max_bytes`) are TCP behaviour exercised through buggify. The mask does not touch them.

`PartitionStrategy` picks the victims: `Random`, `UniformSize`, `IsolateSingle`, `IsolateZone`, `IsolateDatacenter`, `AsymmetricSend`, `AsymmetricRecv`. The locality strategies need `cluster(..)` topology.

`NetworkFaultMask::all()`, `none()`, `.with(fault)`, `.without(fault)`, `.contains(fault)`. Recovery mode (`SimWorld::enter_recovery_mode`) is the `none()` mask plus healing of held partitions.

### Manual network control

| Method | On | Effect |
|--------|----|--------|
| `partition_pair(from, to, duration)` | `SimWorld` | Drop sends from `from` to `to` for `duration`. |
| `partition_send_from(ip, duration)` | `SimWorld` | Drop every send from `ip`. |
| `partition_recv_to(ip, duration)` | `SimWorld` | Drop every send to `ip`. |
| `restore_partition(from, to)` | `SimWorld` | Heal one direction. |
| `is_partitioned(from, to)` | `SimWorld` | Query. |
| `partition(a, b)`, `heal_partition(a, b)`, `is_partitioned(a, b)` | `FaultContext` | The same, by IP string, from a custom injector. |

## Storage Faults

Enabled by `Chaos::Storage(mode)`, limited by `storage_fault_mask`. Fault coordinates are a file path plus a flat 512-byte sector offset.

| `StorageFault` | What happens | Config fields | Reported as |
|----------------|--------------|---------------|-------------|
| `Corruption` | A read or write plants a latent, deterministic sector corruption. | `read_corruption_probability`, `write_corruption_probability` | `StorageFaultKind::ReadCorruption`, `WriteCorruption` |
| `Eio` | A read or write fails with an I/O error. The bytes are not corrupt. | `read_eio_probability`, `write_eio_probability` | `EioRead`, `EioWrite` |
| `Misdirect` | A read is served from, or a write lands at, the wrong offset. | `misdirect_read_probability`, `misdirect_write_probability` | `MisdirectedRead`, `MisdirectedWrite` |
| `PhantomWrite` | A write reports success and never reaches the disk. | `phantom_write_probability` | `PhantomWrite` |
| `SyncFailure` | `sync_all`, `sync_data` or `sync_dir` fails. | `sync_failure_probability` | `SyncFailure` |
| `ShortTransfer` | A read or write transfers fewer bytes than asked. | `short_transfer_probability` | |
| `DirEntryLoss` | A crash loses a directory entry that was not `sync_dir`'d. | `unsynced_dir_entry_loss_probability` | `DirEntryLost` |
| `CrashDamage` | Unsynced sectors resolve by crash physics (below). | `crash_lost_probability`, `crash_latent_fault_probability`, `shorn_write_probability` | `FileCrashReport` |
| `Degradation` | Disk stall (operations wait) or throttle (iops and bandwidth divided) episodes. | `disk_stall_*`, `disk_throttle_*` | |
| `DiskFailure` | The disk fails for good. Every later operation stays pending forever. | `disk_failure_probability` | `storage_disk_failure` event |

The barrier-violation family (`barrier_violation_probability`) is separate from the mask. When it is 0, a sector a sync reported durable that changes across a crash fails the run with `StorageFaultKind::LostSyncedWrite`. This is the **lost-synced-write oracle**. When it is above 0, the lie is permitted and recorded.

`StorageFaultMask::all()`, `none()`, `.with(fault)`, `.without(fault)`, `.contains(fault)`. `StorageConfiguration::disable_fault_injection()` zeroes every family including barrier violation.

### The crash model

A crash (any `RebootKind::Crash`, `CrashAndWipe`, `crash_self`, or a disk crash during `simulate_crash_for_process`) resolves every sector written since the last sync independently:

| `CrashOutcome` | Meaning |
|----------------|---------|
| `KeptOld` | The sector holds the bytes before the write. |
| `KeptNew` | The write survived. |
| `Lost` | The sector holds the fill pattern (`garbage_fill_probability` chooses garbage over zeros). |
| `LatentFault` | The sector reads back with an error until rewritten. |
| `Shorn` | Part of the sector is old, part new. |

`clean_crash_probability` makes some crashes lose nothing. `correlated_rollback_probability` and `correlated_rollback_max_run` roll back a run of adjacent sectors together. `length_survives_crash_probability` decides whether an unsynced `set_len` survives. `FileCrashReport` lists the outcome per sector; `SimWorld::take_storage_crash_reports()` drains them.

### Targeted injection

| Method | On | Effect |
|--------|----|--------|
| `corrupt_file(path, sectors)` | `SimWorld` | Corrupt sectors of a file now. |
| `fail_file_with_eio(path, sectors, EioTarget)` | `SimWorld` | Make reads, writes or both fail on those sectors. |
| `clear_file_eio(path, EioTarget)` | `SimWorld` | Lift it. |
| `corrupt_durable_out_of_band(path, sector)` | `SimWorld` | Corrupt a synced sector without a crash (bit rot). |
| `corrupt_process_file_bytes(ip, path, bytes)` | `SimWorld` | Owner-scoped byte range corruption. |
| `fail_process_file_with_eio(ip, path, sectors, EioTarget)`, `clear_process_file_eio` | `SimWorld` | Owner-scoped EIO. |
| `process_file_durable_bytes(ip, path)` | `SimWorld` | The durable image of a file. |
| `durable_bytes(ip, path)`, `corrupt_bytes(..)`, `fail_file_with_eio(..)`, `clear_file_eio(..)` | `FaultContext` | The same from a custom injector. |
| `simulate_crash_for_process(ip, close_files)` | `SimWorld` | Run crash resolution on a process's files. |
| `wipe_storage_for_process(ip)` | `SimWorld` | Delete every file the process owns. |
| `fail_disk_for_process(ip)` | `SimWorld` | Permanent disk failure. `is_disk_failed(ip)` queries. |
| `set_storage_eligibility_mask(mask)`, `clear_storage_eligibility_mask()` | `SimWorld` | Gate the random families by `(path, sector)`. |
| `set_fault_focus(focus)`, `clear_fault_focus()`, `publish_layout(..)` | `SimWorld` | Weight the random families by byte range or `LayoutRegion`. |
| `draw_replicated_faults(config, localities)` | `SimWorld` | Draw a `FaultPattern` for a replicated group. |
| `take_storage_fault_records()` | `SimWorld` | Drain `StorageFaultRecord { path, kind, .. }` entries. |

`EioTarget` is `Read`, `Write` or `ReadWrite`.

### Replicated fault patterns

`replicated_storage_faults(ReplicatedFaults::new(DomainLevel::Machine).tolerance(1).group("acceptors"))` bounds damage so that a correct replication layer can always repair:

| `FaultPattern` | Damage |
|----------------|--------|
| `Minority { domains }` | Only the listed domains, at most `tolerance`. |
| `Striped { domains, local }` | Stripe `s` may be damaged in the `s mod n`-th domain; the `local` domains anywhere. |
| `Rolling { domains }` | A window of `tolerance` domains at a time. The window moves once the damage is repaired. |
| `Spared` | Fewer than two domains: nothing is damaged. |

The stripe key comes from `LayoutRegion::stripe` (the journal uses the entry position), so one pattern hits the same position on every replica.

## Process Faults

| Fault | Source | What happens |
|-------|--------|--------------|
| Graceful reboot | `Attrition` (`prob_graceful`), `FaultContext::reboot(ip, RebootKind::Graceful)` | The shutdown token is cancelled. After the grace period the process is force killed, then restarted after the recovery delay. |
| Crash | `prob_crash`, `RebootKind::Crash`, `FaultContext::crash(ip)` | Tasks aborted, connections reset, storage through crash resolution. |
| Crash and wipe | `prob_wipe`, `RebootKind::CrashAndWipe` | Crash plus deletion of every owned file. |
| Timed crash | `FaultContext::crash_for(ip, down)` | Crash, restart after `down`. |
| Restart | `FaultContext::restart(ip)` | Restart a process that `crash` left down. |
| Random victim | `FaultContext::reboot_random(kind)`, `reboot_tagged(..)` | One random eligible process. |
| Domain reboot | `FaultContext::reboot_machine(..)`, `reboot_domain(..)`; `AttritionScope::{PerMachine, PerZone, PerDatacenter}` | Every process of the domain at once. |
| Self crash | `SimContext::crash_self(kind, Option<Duration>)`, `SimContext::self_crash() -> SelfCrash` | A process kills itself one scheduler tick from now, at an exact point of its protocol. No randomness is drawn. Only a process may call it. |
| Outage | `Chaos::Outage` | Every process of the named groups loses power at one instant. A workload can hold it with `OutageVeto`. |
| Hint | `hint!("label")` in library code | The seed's first admitting attrition regime may reboot the process at that moment. A workload can budget it with `HintVeto::new(\|ip, label\| bool).publish(state)` under `HINT_VETO_KEY`. |

Reboots are bounded by `max_dead` per injector pool. The restart runs the process factory again: a fresh instance, the same IP, the same disk unless wiped.

## CPU and Gray Failures

| Fault | Source | What happens |
|-------|--------|--------------|
| CPU time | `Chaos::Cpu(mode)`, `SimWorld::set_cpu_model` | Every poll, sent byte and written byte costs simulated time on the process's one core. `cpu_charge(Duration)` adds explicit work. |
| Gray failure | `Chaos::GrayFailure { config, mode }` | Up to `max_slow` machines run slow for a long window: CPU, disk and network multipliers. |
| Manual slowness | `FaultContext::set_slowness(ip, Slowness)`, `SimWorld::set_slowness` | Install multipliers by hand. `SimContext::slowness(ip)` reads them. |

## Buggify Knob Spikes

`Chaos::BuggifyKnobs` calls `apply_buggify_knobs()` on the enabled network and storage profiles for some seeds, the way FoundationDB randomizes knobs under `BUGGIFY`. It has no effect on a surface that is off.

| Surface | Knob | Spiked range |
|---------|------|--------------|
| Network | `clog_probability` | 0.5 to 1.0 |
| Network | `partition_probability` | 0.3 to 0.8 |
| Network | `random_close_probability` | 0.01 to 0.1 |
| Network | `black_hole_probability` | 0.01 to 0.1 |
| Network | `bit_flip_probability` (if already on) | 0.001 to 0.01 |
| Storage | `iops` | 100 to 5000 |
| Storage | `bandwidth` | 1 MB/s to 20 MB/s |
| Storage | `sync_failure_probability` | 0.05 to 0.2 |
| Storage | `disk_stall_probability`, `disk_stall_duration` | 0.1 to 0.5, 100 ms to 500 ms |
| Storage | `disk_throttle_probability`, `disk_throttle_duration` | 0.1 to 0.5, 1 s to 5 s |
| Storage | `disk_failure_probability` | 0.001 to 0.01 |

## Fault Events in the Timeline

The engine records `SimFaultEvent`s. The runner merges them into the trace under `SIM_FAULT_EVENT_NAME` (`"sim_fault"`) with `source = "sim"` and a `kind` field. An invariant queries them with `q.since("sim_fault", &cursor)`.

| `kind` | Family |
|--------|--------|
| `process_graceful_shutdown`, `process_force_kill`, `process_restart` | process lifecycle |
| `partition_created`, `partition_healed` | directed partitions |
| `send_partition_created`, `send_partition_healed`, `recv_partition_created`, `recv_partition_healed` | one-sided partitions |
| `random_close`, `black_hole`, `bit_flip` | connection faults |
| `storage_read_fault`, `storage_write_fault`, `storage_sync_fault` | storage I/O faults |
| `storage_disk_failure`, `storage_crash`, `storage_wipe` | disk and crash |

`SimWorld::take_faults()` drains them in a low-level test.

## Presets

| Preset | Use |
|--------|-----|
| `NetworkConfiguration::default()` | FoundationDB's always-on rates: rare random closes, clock drift, buggified delays, probabilistic connect failures. No clogs, partitions, bit flips or black holes. |
| `NetworkConfiguration::fast_local()` | Microsecond latencies, chaos disabled. |
| `ChaosConfiguration::disabled()` | Every network fault off. |
| `StorageConfiguration::default()` | Realistic SSD timing, no faults. |
| `StorageConfiguration::fast_local()` | 1 µs operations, no faults. |
| `random_for_seed()` / `swarm_for_seed()` | What `ChaosMode::Random` and `ChaosMode::Swarm` draw. |
