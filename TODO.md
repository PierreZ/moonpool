# StorageProvider Implementation Plan

## Context

Implement a `StorageProvider` trait for moonpool enabling deterministic storage simulation with comprehensive fault injection.

**Design choices:**
- **FDB-style interface**: File-oriented with `AsyncRead`/`AsyncWrite`/`AsyncSeek` (tokio traits)
- **TigerBeetle-style fault injection**: Pristine memory + fault bitmap separation
- **Real path strings**: Match production API, backed by in-memory storage in simulation
- **No chaos filtering**: All files subject to faults

## Fault Coverage

Based on [Phil Eaton's disk I/O failure modes](https://notes.eatonphil.com/2025-03-27-things-that-go-wrong-with-disk-io.html):

| Fault | Source | Description |
|-------|--------|-------------|
| **Read corruption** | TigerBeetle | Probabilistic bit corruption on read, deterministic per sector |
| **Write corruption** | TigerBeetle | Probabilistic bit corruption after write completes |
| **Crash/Torn writes** | TigerBeetle | Pending unsynced writes corrupted on crash |
| **Misdirected writes** | TigerBeetle | Data written to wrong offset (overlay system) |
| **Misdirected reads** | Article | Data read from wrong offset |
| **Uninitialized reads** | TigerBeetle | Random data for never-written sectors |
| **IOPS/bandwidth timing** | FDB | Realistic latency: `1/iops + size/bandwidth` |
| **fsync failure** | Article | Sync fails with EIO, ambiguous which write failed |
| **Phantom writes** | Article | Write returns success but data not persisted |

## Key Design Patterns

1. **Pristine + Fault Bitmap**: Clean data stored separately from fault state. Faults toggleable without data loss.
2. **Deterministic Corruption**: Same pristine data seed = same corruption pattern. Retries don't help.
3. **Overlay System**: Misdirected data tracked via overlays, pristine memory unchanged.

---

## Architecture

```
moonpool-core/src/
└── storage.rs              # StorageProvider + StorageFile traits, TokioStorageProvider

moonpool-sim/src/
├── storage/
│   ├── mod.rs              # Module exports
│   ├── config.rs           # StorageConfiguration
│   ├── provider.rs         # SimStorageProvider
│   ├── file.rs             # SimStorageFile (AsyncRead/Write/Seek)
│   ├── memory.rs           # InMemoryStorage (pristine + faults + overlays)
│   └── futures.rs          # SyncFuture, OpenFuture
├── sim/
│   ├── world.rs            # Add storage state + methods
│   └── events.rs           # Add StorageOperation events
└── providers/
    └── sim_providers.rs    # Add SimStorageProvider to bundle
```

---

## Task List

Each task: implement → `cargo fmt` → `cargo clippy` → `cargo nextest run`

**Commit after each phase**, not after each task. **Update progress in this file before committing.**

### Phase 1: Core Traits (moonpool-core) ✓

- [x] **1.1** Create `storage.rs` with `OpenOptions` struct

- [x] **1.2** Define `StorageProvider` trait
  - `open`, `exists`, `delete`, `rename`

- [x] **1.3** Define `StorageFile` trait
  - Supertraits: `AsyncRead + AsyncWrite + AsyncSeek + Unpin`
  - Methods: `sync_all`, `sync_data`, `size`, `set_len`

- [x] **1.4** Implement `TokioStorageProvider` + `TokioStorageFile`

- [x] **1.5** Update `Providers` trait with `Storage` associated type

- [x] **1.6** Update `lib.rs` exports

### Phase 2: Storage Configuration (moonpool-sim) ✓

- [x] **2.1** Create `storage/mod.rs`

- [x] **2.2** Create `storage/config.rs` with `StorageConfiguration`
  ```rust
  pub struct StorageConfiguration {
      // Timing (FDB formula)
      pub iops: u64,                        // Default: 25_000
      pub bandwidth: u64,                   // Default: 150_000_000
      pub read_latency: DurationRange,
      pub write_latency: DurationRange,
      pub sync_latency: DurationRange,

      // Faults (TigerBeetle + Article gaps)
      pub read_fault_probability: f64,
      pub write_fault_probability: f64,
      pub crash_fault_probability: f64,
      pub misdirect_write_probability: f64,
      pub misdirect_read_probability: f64,
      pub phantom_write_probability: f64,
      pub sync_failure_probability: f64,
  }
  ```

### Phase 3: In-Memory Storage (moonpool-sim) ✓

- [x] **3.1** Create `storage/memory.rs` with `InMemoryStorage`
  - `data: Vec<u8>` (pristine)
  - `written: BitSet` (512B sectors)
  - `faults: BitSet`

- [x] **3.2** Implement `read()` with fault application
  - Copy pristine data
  - Fill unwritten sectors with deterministic random
  - Apply corruption (deterministic seed from pristine bytes)

- [x] **3.3** Implement `write()`
  - Clear faults, copy to pristine, mark written

- [x] **3.4** Add misdirection overlay system (write misdirection)

- [x] **3.5** Add misdirected read support

- [x] **3.6** Add phantom write tracking

- [x] **3.7** Implement `apply_crash()` for torn writes

### Phase 4: Storage Events (moonpool-sim) ✓

- [x] **4.1** Create `storage/events.rs` with `StorageOperation`

- [x] **4.2** Add `Storage(StorageOperation)` to `Event` enum

### Phase 5: SimWorld Integration (moonpool-sim) ✓

- [x] **5.1** Add storage state to `SimInner`

- [x] **5.2** Add `StorageFileState` struct

- [x] **5.3** Implement file management methods

- [x] **5.4** Implement I/O request methods

- [x] **5.5** Implement latency calculation (FDB formula)

- [x] **5.6** Implement `simulate_crash()`

- [x] **5.7** Handle storage events in `step()`

### Phase 6: SimStorageProvider (moonpool-sim) ✓

- [x] **6.1** Create `storage/provider.rs`

- [x] **6.2** Implement `open()` with latency scheduling

- [x] **6.3** Implement `exists()`, `delete()`, `rename()`

### Phase 7: SimStorageFile (moonpool-sim) ✓

- [x] **7.1** Create `storage/file.rs` with `SimStorageFile` (stub created in Phase 6)

- [x] **7.2** Implement `AsyncRead` with fault injection

- [x] **7.3** Implement `AsyncWrite` with phantom/misdirection

- [x] **7.4** Implement `AsyncSeek`

- [x] **7.5** Implement `StorageFile` trait (sync with failure probability)

- [x] **7.6** Create `storage/futures.rs`

### Phase 8: Provider Bundle (moonpool-sim) ✓

- [x] **8.1** Update `SimProviders` with storage field

- [x] **8.2** Implement `Providers` trait

- [x] **8.3** Update `lib.rs` exports

### Phase 9: Testing (Simple, following moonpool patterns) ✅

- [x] **9.1** Create `tests/storage/mod.rs` (storage.rs + storage/ directory)

- [x] **9.2** Basic workload tests (8 tests in basic.rs)
  - test_create_write_read, test_seek_operations, test_file_size, test_set_len
  - test_sync_operations, test_file_not_found, test_file_already_exists, test_delete_and_rename

- [x] **9.3** Determinism tests (4 tests in determinism.rs)
  - test_same_seed_same_timing, test_same_seed_same_corruption
  - test_different_seeds_different_timing, test_deterministic_misdirection

- [x] **9.4** Fault trigger tests (8 tests in faults.rs)
  - test_read_corruption_fault, test_write_corruption_fault
  - test_misdirected_write_fault, test_misdirected_read_fault
  - test_phantom_write_fault, test_sync_failure_fault
  - test_crash_torn_writes, test_uninitialized_reads

- [x] **9.5** Latency formula tests (6 tests in latency.rs)
  - test_fast_storage_config, test_default_storage_config
  - test_custom_latency_ranges, test_latency_formula
  - test_read_latency_scales_with_size, test_write_latency_scales_with_size

**Deferred (for file transport layer):**
- Crash-recovery workflows
- Checksum validation workloads
- Multi-file scenarios
- State machine testing

### Phase 10: Documentation

- [ ] **10.1** Doc comments on public items

- [ ] **10.2** Update CLAUDE.md with storage patterns

---

## How to Write Simulation Tests

### The Step Loop Pattern

Storage operations return `Poll::Pending` and require simulation stepping (unlike network which buffers and returns `Poll::Ready` immediately). The correct pattern:

1. `spawn_local` the async workload
2. Loop: `sim.step()` → check `is_finished()` → `yield_now()`
3. Await the handle when finished

### Example

```rust
#[test]
fn test_storage_operation() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async {
        let mut sim = SimWorld::new();
        let provider = sim.storage_provider();

        // Spawn the async workload as a local task
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider.open("test.txt", OpenOptions::create_write()).await?;
            file.write_all(b"hello").await?;
            file.sync_all().await?;
            Ok::<_, std::io::Error>(())
        });

        // Step loop: process simulation events + yield to tasks
        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        handle.await.unwrap().unwrap();
    });
}
```

### Key Functions

- `sim.step()` - Process one event
- `sim.run_until_empty()` - Process all events (network-style, may not work for storage)
- `sim.pending_event_count()` - Check pending events
- `tokio::task::yield_now()` - Let spawned tasks run
- `handle.is_finished()` - Check task completion
- `sim.storage_provider()` - Create a storage provider for this simulation

---

## Verification

After each task:
```bash
nix develop --command cargo fmt
nix develop --command cargo clippy
nix develop --command cargo nextest run
```
