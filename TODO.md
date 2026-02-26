# Sancov Integration: Incremental Rebuild Plan

## Context

moonpool-explorer uses fork()-based multiverse exploration with an assertion-level coverage bitmap (8192 bits) to decide when to fork. We're adding LLVM SanitizerCoverage `inline-8bit-counters` as an **additional** coverage signal — when a child timeline discovers new code edges in the code under test, the explorer considers it productive and forks more.

**Critical design principle**: Sancov instruments everything **built on top of** the simulation framework — the user's application, and the moonpool libraries that the application uses (`moonpool`, `moonpool-transport`). What's **excluded** is the testing infrastructure itself: `moonpool_sim` (simulation runtime) and `moonpool_explorer` (fork machinery). Their code paths are noise — the same chaos injection and fork logic fire regardless of whether the target is doing interesting things. Proc-macros (`moonpool-transport-derive`) are host code and can't be instrumented anyway. `moonpool-core` is just trait definitions with minimal executable code.

**Key constraint**: `__sanitizer_cov_8bit_counters_init` stores pointers in TLS during static init. libtest runs tests in worker threads where this TLS is invisible. **Sancov only works in binary targets, not `cargo test`.**

**Branch**: `dev/pz/code_coverage` — 5 incremental commits.

---

## ~~Commit 1: `sancov.rs` core module~~ ✅ DONE

Add the sancov module to moonpool-explorer with no fork loop integration yet.

### Files
- **Create** `moonpool-explorer/src/sancov.rs`
- **Modify** `moonpool-explorer/src/lib.rs` — add `pub mod sancov` + re-exports

### sancov.rs contents

**LLVM callbacks** (called during static init, once per compilation unit):
```rust
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __sanitizer_cov_8bit_counters_init(start: *mut u8, stop: *mut u8)
// Merges ranges via min(start)/max(stop) across callbacks.
// With -Ccodegen-units=1, one callback per instrumented crate.
// Gaps between crate BSS sections contain zeros (skipped by novelty detector).

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __sanitizer_cov_pcs_init(_beg: *const usize, _end: *const usize)
// Stub — PC table unused for now
```

**Global statics** (set during static init, before `main()`):
- `COUNTERS_PTR: AtomicPtr<u8>` — BSS counter array pointer
- `COUNTERS_LEN: AtomicUsize` — edge count

These use `static Atomic*` (not thread-local `Cell`) because `__sanitizer_cov_8bit_counters_init`
is called by LLVM during static constructors, before Rust's runtime is fully initialized.
`Atomic*` statics are zero-initialized at compile time and require no runtime setup.

**Thread-local state** (set during `init()` from `main()`):
- `SANCOV_TRANSFER: Cell<*mut u8>` — MAP_SHARED child→parent buffer
- `SANCOV_HISTORY: Cell<*mut u8>` — MAP_SHARED global max map
- `SANCOV_POOL: Cell<*mut u8>` — parallel mode pool base
- `SANCOV_POOL_SLOTS: Cell<usize>` — pool slot count

**AFL bucketing** — `COUNT_CLASS_LOOKUP[256]`:
```
0→0, 1→1, 2→2, 3→4, 4-7→8, 8-15→16, 16-31→32, 32-127→64, 128-255→128
```
From LibAFL `libafl/src/observers/map/hitcount_map.rs:22-36`. Applied via `classify_counts()`.

**Novelty detection** — `has_new_coverage_inner(buffer, history, len) -> bool`:
- Skip zeros (unexecuted edges)
- For each edge: if `bucketed_current > history[i]`, update history, return true
- This is the max-reduce from LibAFL `libafl_bolts/src/simd.rs:437-473`

**Public API**: `sancov_is_available()` (checks `COUNTERS_PTR.load(Relaxed).is_null()`), `sancov_edge_count()`, `sancov_edges_covered()`

**Lifecycle**: `init_sancov_shared()`, `cleanup_sancov_shared()`, `clear_transfer_buffer()`

**Child transfer**: `copy_counters_to_shared()` — memcpy BSS→transfer before `_exit()`

**BSS counter reset**: `reset_bss_counters()` — zero BSS counters in child after fork:
```rust
pub fn reset_bss_counters() {
    let ptr = COUNTERS_PTR.load(Ordering::Relaxed);
    if !ptr.is_null() {
        let len = COUNTERS_LEN.load(Ordering::Relaxed);
        unsafe { std::ptr::write_bytes(ptr, 0, len); }
    }
}
```
This is needed because after `fork()`, children inherit the parent's accumulated BSS counters.
Without zeroing, parent-accumulated values crossing AFL bucket boundaries cause spurious
novelty detections at deeper fork depths — wasting fork budget on unproductive marks.

**No-op safety**: All public functions must early-return when `sancov_is_available()` is false
(i.e. `COUNTERS_PTR` is null). This ensures graceful degradation when running without
`SANCOV_CRATES` — no null pointer dereferences.

**Parallel pool**: `get_or_init_sancov_pool(slot_count)`, `sancov_pool_slot(base, idx)`, `has_new_sancov_coverage_from(slot_ptr)`

**Tests**: bucketing, novelty, known-skip, higher-bucket novelty, unavailable no-op, init/cleanup lifecycle

**Re-exports from lib.rs**:
```rust
pub use sancov::{sancov_edge_count, sancov_edges_covered, sancov_is_available};
```

---

## ~~Commit 2: Build infrastructure — SANCOV_CRATES~~ ✅ DONE

Build infrastructure comes before fork loop integration so that commit 3 can be
end-to-end tested with actual sancov instrumentation.

### Decision: RUSTC_WRAPPER script (per-crate whitelist)

Instrumenting `moonpool_sim` is actively harmful: different seeds and chaos injection paths produce genuinely novel edges in the sim runtime, triggering forks that have nothing to do with the code under test. Per-crate selectivity is required.

### Files

**Create `scripts/sancov-rustc.sh`**:
- When `SANCOV_CRATES` is unset/empty → pass through to rustc unchanged (no-op)
- Skip `build_script_build` and `--crate-type proc-macro` unconditionally
- Parse `--crate-name`, check against `SANCOV_CRATES` (comma-separated whitelist)
- Matching crates get: `-Cpasses=sancov-module -Cllvm-args=-sanitizer-coverage-level=3 -Cllvm-args=-sanitizer-coverage-inline-8bit-counters -Ccodegen-units=1`

**Modify `flake.nix`**:
- Set `RUSTC_WRAPPER` pointing to the script (always active, no-op when `SANCOV_CRATES` unset)

**Usage** — `SANCOV_CRATES` is the only knob:
```bash
# Example crate only (maze/dungeon logic is self-contained):
SANCOV_CRATES=moonpool_explorer_examples cargo run --bin maze_explore

# Real application — instrument app + framework libraries:
SANCOV_CRATES=my_app,moonpool,moonpool_transport cargo run --bin my_app

# Without sancov (script passes through):
cargo run --bin maze_explore
```

---

## ~~Commit 3: Fork loop integration~~ ✅ DONE

Wire sancov into `split_loop.rs` so it contributes to fork decisions.

### Files
- **Modify** `moonpool-explorer/src/split_loop.rs`
- **Modify** `moonpool-explorer/src/lib.rs` — init/cleanup/prepare_next_seed hooks

### Integration points

**`exit_child()`** — copy counters before `_exit()`:
```rust
pub fn exit_child(code: i32) -> ! {
    crate::sancov::copy_counters_to_shared();
    unsafe { libc::_exit(code) }
}
```

**`setup_child()`** — reset BSS counters + sancov pool for nested splits:
```rust
// Zero BSS counters so child captures only its OWN edges, not parent's accumulated noise.
// Without this, parent-accumulated counters crossing AFL bucket boundaries cause
// spurious novelty detections at deeper fork depths.
crate::sancov::reset_bss_counters();

// Reset pool so nested splits allocate a fresh pool
crate::sancov::SANCOV_POOL.with(|c| c.set(std::ptr::null_mut()));
crate::sancov::SANCOV_POOL_SLOTS.with(|c| c.set(0));
```

**Sequential fork** (in both `split_on_discovery` and `adaptive_split_on_discovery`):
- Before fork: `crate::sancov::clear_transfer_buffer()`
- After waitpid: `batch_has_new |= crate::sancov::has_new_sancov_coverage()`

**Parallel fork** (sliding window):
- Init: `sancov_pool_base = crate::sancov::get_or_init_sancov_pool(slot_count)`
- Save: `parent_sancov_transfer = SANCOV_TRANSFER.with(|c| c.get())`
- Per child: clear slot, redirect `SANCOV_TRANSFER` to pool slot
- `reap_one()` gains `sancov_pool_base` param, checks `has_new_sancov_coverage_from(slot_ptr)`
- After loop: restore parent's transfer pointer

**`init()`** — add `sancov::init_sancov_shared()?` after `init_assertions()`
**`cleanup()`** — add `sancov::cleanup_sancov_shared()` before `cleanup_assertions()`
**`prepare_next_seed()`** — add `sancov::clear_transfer_buffer()` + `sancov::reset_bss_counters()` (preserves history map, avoids counter wrapping noise across seeds)

---

## ~~Commit 4: Stats & reporting~~ ✅ DONE

### Files
- **Modify** `moonpool-explorer/src/shared_stats.rs` — add to `ExplorationStats`:
  ```rust
  pub sancov_edges_total: usize,
  pub sancov_edges_covered: usize,
  ```
  Populated from `crate::sancov::sancov_edge_count()` and `sancov_edges_covered()`.

- **Modify** `moonpool-sim/src/runner/report.rs` — add to `ExplorationReport`:
  ```rust
  pub sancov_edges_total: usize,
  pub sancov_edges_covered: usize,
  ```

- **Modify** `moonpool-sim/src/runner/builder.rs` — populate from `final_stats`

- **Modify** `moonpool-sim/tests/exploration/maze.rs` — print sancov stats

---

## ~~Commit 5: Move simulation tests to binary targets~~ ✅ DONE

Sancov only works in binary targets (`main()` runs on the main thread where TLS from static init is visible). `cargo test` uses worker threads where `__sanitizer_cov_8bit_counters_init` TLS is invisible. Moving all `slow_simulation_*` tests to binary targets enables sancov-guided exploration and eliminates the test-vs-binary duplication problem.

### Source tests to move (7 files, 4 crates)

| Source file | Tests |
|---|---|
| `moonpool-sim/tests/exploration/maze.rs` | `slow_simulation_maze`, `slow_simulation_maze_bug_replay` |
| `moonpool-sim/tests/exploration/dungeon.rs` | `slow_simulation_dungeon`, `slow_simulation_dungeon_bug_replay` |
| `moonpool/tests/metastable/tests.rs` | `slow_simulation_metastable_failure` |
| `moonpool/tests/simulation/test_scenarios.rs` | `slow_simulation_banking_chaos` (ignored) |
| `moonpool-transport/tests/e2e/tests.rs` | `slow_simulation_reliable_delivery`, `slow_simulation_unreliable_drops`, `slow_simulation_mixed_queues`, `slow_simulation_reconnection` |
| `moonpool-transport/tests/simulation/test_scenarios.rs` | `slow_simulation_local_delivery`, ... |
| `moonpool-explorer/tests/adaptive_scenarios.rs` | `slow_simulation_adaptive_maze_cascade` |

### Files

**Create `moonpool-simulations/Cargo.toml`** — deps: `moonpool`, `moonpool-sim`, `moonpool-explorer`, `moonpool-transport`, `async-trait`, `tokio`, `tracing`, `tracing-subscriber`, `serde`, `serde_json`

**Create `moonpool-simulations/src/lib.rs`** — shared `run_simulation()` harness + report validation:
```rust
pub fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");
    rt.block_on(async { builder.run().await })
}
```

**Create workload modules** (moved from test files):
- `moonpool-simulations/src/maze.rs` — `Maze` struct + `MazeWorkload` (from `moonpool-sim/tests/exploration/maze.rs`)
- `moonpool-simulations/src/dungeon.rs` — `Dungeon` + `DungeonWorkload` (from `moonpool-sim/tests/exploration/dungeon.rs`)
- `moonpool-simulations/src/metastable.rs` — metastable workloads (from `moonpool/tests/metastable/`)
- `moonpool-simulations/src/banking.rs` — banking workloads (from `moonpool/tests/simulation/`)
- `moonpool-simulations/src/transport.rs` — transport workloads (from `moonpool-transport/tests/e2e/` + `simulation/`)
- `moonpool-simulations/src/adaptive.rs` — adaptive maze (from `moonpool-explorer/tests/adaptive_scenarios.rs`)

**Create binary targets** — one per simulation scenario:
- `moonpool-simulations/src/bin/maze_explore.rs`
- `moonpool-simulations/src/bin/dungeon_explore.rs`
- `moonpool-simulations/src/bin/metastable_explore.rs`
- `moonpool-simulations/src/bin/banking_chaos.rs`
- `moonpool-simulations/src/bin/transport_e2e.rs`
- `moonpool-simulations/src/bin/adaptive_maze.rs`

Each binary: configure `SimulationBuilder`, call `run_simulation()`, print report, `process::exit(1)` on assertion violations.

Bug replay tests (`slow_simulation_*_bug_replay`) become `#[test]` in the simulations crate — they're fast (no exploration) and don't need sancov.

**Delete original test files** from source crates:
- `moonpool-sim/tests/exploration/{maze,dungeon}.rs` + `mod.rs`
- `moonpool/tests/metastable/tests.rs` (keep non-slow tests if any)
- `moonpool/tests/simulation/test_scenarios.rs` (keep non-slow tests if any)
- `moonpool-transport/tests/e2e/tests.rs` (keep non-slow tests if any)
- `moonpool-transport/tests/simulation/test_scenarios.rs` (keep non-slow tests if any)
- `moonpool-explorer/tests/adaptive_scenarios.rs`

**Modify root `Cargo.toml`** — add `moonpool-simulations` to workspace members

**Update `.config/nextest.toml`** — adjust profiles now that `slow_simulation_*` tests no longer exist as cargo test targets

### Usage

```bash
# Run a specific simulation:
cargo run --bin maze_explore

# With sancov:
SANCOV_CRATES=moonpool_simulations,moonpool,moonpool_transport cargo run --bin maze_explore

# Run all simulations:
cargo run --bin maze_explore && cargo run --bin dungeon_explore && ...

# Fast unit tests (unchanged):
cargo test-fast
```

---

## Verification

For each commit:
```bash
nix develop --command cargo fmt
nix develop --command cargo clippy
nix develop --command cargo nextest run --profile fast
```

End-to-end (after commit 5):
```bash
nix develop

# With sancov:
SANCOV_CRATES=moonpool_simulations cargo run --bin maze_explore
# → sancov edges: N/M (available: true)

# Without sancov (RUSTC_WRAPPER passes through when SANCOV_CRATES unset):
cargo run --bin maze_explore
# → sancov edges: 0/0 (available: false)
```

---

## Corrections Applied

Changes from the original plan:

1. **Added `reset_bss_counters()` in commit 1 + called in `setup_child()` in commit 3**: After `fork()`, children inherit parent's accumulated BSS counters. Without zeroing, parent-accumulated values crossing AFL bucket boundaries at deeper fork depths cause spurious novelty detections that waste fork budget. Zeroing in `setup_child()` ensures each child captures only its own edges.

2. **Reordered commits: build infra (was 4) → now commit 2**: The RUSTC_WRAPPER script is needed before fork loop integration (was commit 2, now 3) can be end-to-end tested. Without sancov compiler flags, `__sanitizer_cov_8bit_counters_init` is never called and integration code is dead.

3. **Added `reset_bss_counters()` in `prepare_next_seed()`**: Parent BSS counters accumulate across seeds. 8-bit counters can wrap (255→0). Resetting prevents stale counter noise between seeds.

4. **Made no-op guards explicit in commit 1**: All sancov public functions must early-return when `sancov_is_available()` is false. Prevents null pointer dereferences when running without `SANCOV_CRATES`.

5. **Fixed `SANCOV_CRATES` documentation**: Added general-case usage example (`SANCOV_CRATES=my_app,moonpool,moonpool_transport`) alongside the example-only shorthand, to match the stated design principle.

6. **Changed COUNTERS_PTR/COUNTERS_LEN to `static Atomic*`**: The LLVM callback `__sanitizer_cov_8bit_counters_init` runs during static constructors (before `main()`). Thread-local `Cell` requires Rust's `thread_local!` machinery; `static AtomicPtr`/`AtomicUsize` are zero-initialized at compile time and universally safe for pre-main use. MAP_SHARED buffer pointers (`SANCOV_TRANSFER`, `SANCOV_HISTORY`, `SANCOV_POOL`, `SANCOV_POOL_SLOTS`) remain `Cell` since they're set during `init()` from `main()`.

7. **Documented range merging algorithm**: `min(start)/max(stop)` across multiple `__sanitizer_cov_8bit_counters_init` callbacks. With `-Ccodegen-units=1`, one callback per instrumented crate. Gaps between BSS sections contain zeros, skipped by novelty detector.
