# Migration: moonpool from `?Send` to Send-bounded traits

> **For future sessions** — this file is the source of truth. The plan was
> approved on 2026-05-20. Update the checkboxes as work progresses and commit
> the TODO.md changes alongside each phase commit so the next session sees
> current state. If you are picking this up cold, read this entire file first.

## Why this migration

moonpool is being evaluated to become the primary simulation library at Clever
Cloud. Internal customers (e.g. the BDS service) already write production code
with `Arc<RwLock<…>>`, `Arc<AtomicBool>`, `DashMap`, `tokio::sync::Semaphore`,
and 11+ `tokio::spawn` sites that require `Send + 'static`. Today moonpool's
traits are `#[async_trait(?Send)]`, which would force every customer to
refactor their call graph to `!Send` just to integrate. An internal review
estimated BDS integration at **3-4/5 effort if moonpool stays `?Send`, dropping
to 2/5 ("almost purely mechanical") if moonpool migrates to Send.**

Determinism in simulation is non-negotiable.

## The key insight

`Builder::new_current_thread().build()` (not `.build_local()`) returns a
`Runtime` that:

- Runs all tasks on **exactly one OS thread** (the thread calling `block_on`).
- Exposes `Runtime::spawn<F: Future + Send + 'static>` — Send-bounded.
- Polls tasks FIFO from `VecDeque<Notified>` (no LIFO slot,
  no work-stealing, no thread handoff at `block_on`).
- `Builder::rng_seed(...)` deterministically seeds internal PRNG.

The current `.build_local()` and the proposed `.build()` differ only by storing
a `ThreadId` and wrapping in a `!Send` newtype — the underlying scheduler,
queue, polling order, and drivers are bit-for-bit identical (both call
`build_current_thread_runtime_components(...)`). Switching to `.build()`
preserves determinism and unlocks `tokio::spawn` with `Send + 'static`.

**Bonus**: drops the `tokio_unstable` cfg flag (`build_local` is unstable;
`build` is stable).

## Trait style: AFIT for providers, `#[async_trait]` for dyn-stored traits

Two styles coexist in this migration, each used where it fits best:

- **Native AFIT (async fn in trait)** for provider traits: `TimeProvider`,
  `TaskProvider`, `NetworkProvider`, `StorageProvider`, `RandomProvider`. These
  are used only as concrete types or generic parameters — never as
  `Box<dyn …>` — so they don't need dyn-compatibility. AFIT removes the
  `Pin<Box<dyn Future + Send>>` heap allocation that `#[async_trait]` adds to
  every call. On hot paths like `time.sleep(...)` and
  `task_provider.spawn_task(...)`, that allocation goes away.

- **`#[async_trait]` (no `?Send`)** for traits stored as `Box<dyn …>`:
  `Process`, `Workload`, `FaultInjector`, and the `#[service]` macro's handler
  trait. The orchestrator (`moonpool-sim/src/runner/orchestrator.rs:77,91,120,263,280,281,475`,
  `moonpool-sim/src/runner/builder.rs:31,205,213,218,299,379,556`) stores
  these heterogeneously, and native AFIT traits are not dyn-compatible without
  wrappers. The per-call Box allocation here is acceptable — these methods
  fire once per setup/run/check cycle, not in inner loops.

**Send inference for AFIT**: every provider trait gets `Self: Send + Sync + 'static`
as a supertrait. Because `&self` is then `&(Send + Sync)` (i.e. `Send`), and
because method arguments are required to be `Send`, the compiler infers the
returned opaque future as `Send` automatically — no return-type notation or
manual `+ Send` annotations needed at call sites. Callers just bound
`T: TimeProvider` and the future is usable in `tokio::spawn`.

**`#[async_trait]` traits also get `Send + Sync + 'static` supertrait**, so
`Box<dyn Process>` is automatically `Send` without `Box<dyn Process + Send>`
contortions at use sites.

## Conventions (read before every commit)

- **No GPG signing**: pass `--no-gpg-sign` to every `git commit`. The yubikey
  will time out across a long commit sequence; we accept unsigned commits for
  this branch.
- **Per-crate compile per commit**: after each commit, run only the touched
  crate's checks. Use the workspace-wide build only at phase boundaries.
  ```
  nix develop --command cargo build -p <crate>
  nix develop --command cargo nextest run -p <crate>
  ```
  At phase boundaries (and certainly before pushing):
  ```
  nix develop --command cargo fmt
  nix develop --command cargo clippy -- -D warnings
  nix develop --command cargo nextest run
  ```
- **Interior mutability primitive**: **`std::sync::RwLock`** (not
  `parking_lot::Mutex`, not `parking_lot::RwLock`). Matches customer BDS
  conventions (`Arc<RwLock<…>>`) and adds no dep. Read paths use `.read()`,
  mutation uses `.write()`.
- **Lock poisoning policy**: lock acquisitions use
  `.expect("RwLock poisoned: prior task panicked")`. moonpool's "no `unwrap()`"
  rule targets recoverable errors; poisoning indicates a prior panic and the
  sim should die anyway. `.expect(...)` makes the intent explicit.
- **Atomic counters**: replace `Rc<Cell<usize>>` with `Arc<AtomicUsize>`,
  `Rc<Cell<bool>>` with `Arc<AtomicBool>`. Default to `Ordering::Relaxed` for
  monotonic counters; use `Ordering::AcqRel` when the value gates a downstream
  read/write.
- **Single PR**: `refactor/send-bounded-traits`. All phases land as incremental
  commits inside that PR — do **not** open multiple PRs.
- **Commit messages**: conventional commits (`refactor(<crate>): …`,
  `test(<crate>): …`, `docs: …`). Prefix all migration commits with the
  affected crate.
- **Update this TODO.md as you go**: check boxes off and `git add TODO.md`
  alongside each phase commit. Future sessions read the current state from
  this file.

## Determinism preservation (what is unchanged)

- Sim runtime is still single-OS-thread → FIFO task poll order.
- `BinaryHeap<ScheduledEvent>` with sequence-number tiebreaker in
  `moonpool-sim/src/sim/events.rs:141-194` is unchanged.
- `NetworkState` uses `BTreeMap` everywhere
  (`moonpool-sim/src/sim/state.rs:184-210`) — deterministic iteration.
- Sim-controlled time, seeded ChaCha8Rng (`moonpool-sim/src/sim/rng.rs`),
  thread-local chaos/buggify/assertion state remain thread-local (safe on the
  single-thread sim runtime).
- moonpool-explorer fork happens **between** simulations, not during
  (`moonpool-sim/src/runner/orchestrator.rs:783,790`) — orthogonal to the
  Send choice.

## Customer interop pattern (the goal)

After migration, BDS-style integration is mechanical:

```rust
// Customer production code (unchanged):
tokio::spawn(async move { … });
tokio::time::sleep(Duration::from_secs(1)).await;

// Customer test driver against moonpool:
task_provider.spawn_task("name", async move { … });
time_provider.sleep(Duration::from_secs(1)).await;
```

Customer state stays `Arc<RwLock<…>>` / `DashMap` / `Arc<AtomicBool>`. No
call-graph refactor. No trait juggling.

## Phases

### Phase 0 — Preflight
- [ ] `git status` && check current branch. If on `main` or `master`:
  `git switch -c refactor/send-bounded-traits`. If already on a feature
  branch, stay on it.
- [ ] Capture a determinism baseline: run
  `nix develop --command cargo xtask sim run-all` and save the output to a
  scratch file (e.g. `/tmp/moonpool-baseline-traces.txt`). Phase 6 compares
  against this.
- [ ] Commit this TODO.md to the branch (the `Write` that created it leaves
  it untracked):
  ```
  git add TODO.md && git commit --no-gpg-sign -m "chore: add Send-bounded traits migration TODO"
  ```

### Phase 1 — moonpool-core: migrate provider traits to native AFIT
Scope: convert every provider trait from `#[async_trait(?Send)]` to native AFIT
with `Send + Sync + 'static` supertraits. Drop the `async-trait` dependency
from `moonpool-core/Cargo.toml` entirely.

For each trait, the pattern is:

```rust
// Before:
#[async_trait(?Send)]
pub trait TimeProvider {
    async fn sleep(&self, dur: Duration);
}

// After:
pub trait TimeProvider: Send + Sync + 'static {
    async fn sleep(&self, dur: Duration);
}
```

The `Send + Sync + 'static` supertrait means `&self` is `&(Send + Sync)` which
is `Send`. Combined with `Send` method args, the compiler infers the returned
opaque future as `Send` automatically. No return-type notation needed.

- [ ] `moonpool-core/src/time.rs`: convert `TimeProvider` (line 35) and
  `TimeProviderExt` (line 98) to native AFIT. Remove the `use async_trait::async_trait`
  import. Add `Send + Sync + 'static` supertrait to `TimeProvider`. The
  `TokioTimeProvider` impl just becomes `impl TimeProvider for TokioTimeProvider`
  (no `#[async_trait]` attribute, body unchanged).
- [ ] `moonpool-core/src/task.rs`: convert `TaskProvider` (line 30) and
  `TaskProviderExt` (line 90) to native AFIT with the supertrait. In
  `TokioTaskProvider::spawn_task` (line 94-108), replace
  `tokio::task::Builder::new().spawn_local(...)` with
  `tokio::task::Builder::new().spawn(...)`. Update the bound on `F` from
  `F: Future<Output = ()> + 'static` to
  `F: Future<Output = ()> + Send + 'static`. Update `Self::JoinHandle`
  associated-type bound to `Future<Output = Result<(), JoinError>> + Send + 'static`.
- [ ] `moonpool-core/src/network.rs`: convert `NetworkProvider` (line 16),
  `TcpListener` (line 31), `TcpStream` (line 68), and the stream trait at
  line 91 to native AFIT with the supertrait.
- [ ] `moonpool-core/src/storage.rs`: convert `StorageProvider` (line 100)
  and file traits (lines 119, 155, 206) to native AFIT with the supertrait.
- [ ] `moonpool-core/Cargo.toml`: remove the `async-trait` dependency entry.
- [ ] Compile per-crate:
  `nix develop --command cargo build -p moonpool-core && nix develop --command cargo nextest run -p moonpool-core`
- [ ] Commit:
  `git add -u && git commit --no-gpg-sign -m "refactor(core): migrate provider traits to native AFIT"`

### Phase 2 — moonpool-sim runner + builder
Scope: keep `#[async_trait]` on dyn-stored traits (`Process`, `Workload`,
`FaultInjector`) but drop `?Send` and add `Send + Sync + 'static` supertraits.
Switch the runtime constructor.

For each dyn-stored trait, the pattern is:

```rust
// Before:
#[async_trait(?Send)]
pub trait Process: 'static { ... }

// After:
#[async_trait]
pub trait Process: Send + Sync + 'static { ... }
```

The `Send + Sync` supertrait means `Box<dyn Process>` is automatically
`Send + Sync` — no `Box<dyn Process + Send>` annotations needed at use sites.

- [ ] `moonpool-sim/src/runner/process.rs:44-45` — change
  `#[async_trait(?Send)] / pub trait Process: 'static` to
  `#[async_trait] / pub trait Process: Send + Sync + 'static`.
- [ ] `moonpool-sim/src/runner/workload.rs:37-38` — same change for `Workload`.
- [ ] `moonpool-sim/src/runner/fault_injector.rs:275-276` — same change for
  `FaultInjector`. Also update lines 275, 302 if they carry their own
  `#[async_trait(?Send)]` attributes.
- [ ] `moonpool-sim/src/runner/builder.rs:1208, 1236` — drop `?Send` from
  `TestableProcess` / `TestableWorkload`; add `Send + Sync + 'static`
  supertraits.
- [ ] `moonpool-sim/src/runner/builder.rs:770-774` — replace
  `.build_local(Default::default())` with `.build()`. Drop the `LocalOptions`
  import if present.
- [ ] Note: workspace will not compile cleanly until Phase 3 lands —
  do **not** try to compile yet. Stage and continue.
- [ ] Commit (workspace red is acceptable here — single PR, will go green by
  end of Phase 3):
  `git add -u && git commit --no-gpg-sign -m "refactor(sim): drop ?Send from Process/Workload/FaultInjector, switch to build()"`

### Phase 3 — moonpool-sim internals
Scope: swap interior mutability in the sim core; replace spawn_local.

- [ ] `moonpool-sim/src/sim/world.rs:145` — `SimWorld.inner: Rc<RefCell<SimInner>>`
  → `Arc<RwLock<SimInner>>`.
- [ ] `moonpool-sim/src/sim/world.rs:2382` — `SimHandle.inner: Weak<RefCell<…>>`
  → `Weak<RwLock<SimInner>>`.
- [ ] Fix every borrow site in `moonpool-sim/src/sim/world.rs` and downstream:
  - `inner.borrow()` → `inner.read().expect("RwLock poisoned: prior task panicked")`
  - `inner.borrow_mut()` → `inner.write().expect("RwLock poisoned: prior task panicked")`
  - Watch for the NLL field-disjoint pattern: `RefCell` allowed two borrows
    of disjoint fields under NLL; `RwLock` is whole-object. If you hit it,
    drop the guard early (`drop(guard);`) before the next acquire, or split
    the operation so both fields are touched under one guard. The hot
    orchestration loop in `moonpool-sim/src/sim/world.rs` (search for
    `connections.get_mut` near event_queue scheduling) is the known pinch
    point — extract values into locals before re-acquiring.
- [ ] `moonpool-sim/src/runner/orchestrator.rs:101, 114, 134` —
  `Rc<Cell<usize>>` → `Arc<AtomicUsize>`. Use
  `.load(Ordering::Relaxed)` for reads and `.fetch_add(1, Ordering::Relaxed)`
  for increments.
- [ ] `moonpool-sim/src/runner/orchestrator.rs:234, 361, 411, 493, 509, 746` —
  replace each `tokio::task::spawn_local(...)` with `tokio::spawn(...)`. Every
  captured value in those closures must be `Send + 'static`. If a closure
  captures `SimTcpStream` or similar, defer that fix to Phase 6 and tag the
  commit message; otherwise this phase covers it.
- [ ] `moonpool-sim/src/sim/world.rs:370-371` — review `task_provider()`
  returning `TokioTaskProvider`; should still work.
- [ ] Compile per-crate:
  `nix develop --command cargo build -p moonpool-sim && nix develop --command cargo nextest run -p moonpool-sim`
- [ ] Commit (split if useful):
  `git add -u && git commit --no-gpg-sign -m "refactor(sim): swap Rc<RefCell<SimInner>> for Arc<RwLock<SimInner>>"`
  `git add -u && git commit --no-gpg-sign -m "refactor(sim): swap spawn_local for spawn in orchestrator"`

### Phase 4 — moonpool-transport (bottom-to-top)
Scope: 115 `Rc<…>` + 20 `RefCell<…>` sites. Process **leaf-first** in the
dependency graph; each commit is self-contained and compiles.

For each sub-step: `Rc::new(x)` → `Arc::new(x)`, `RefCell::new(x)` →
`RwLock::new(x)`, borrows as in Phase 3. Compile only `moonpool-transport`
between commits.

- [ ] **4.1 Primitives**: `moonpool-transport/src/rpc/interface_method.rs`,
  `rpc/endpoint_map.rs`, `rpc/request.rs`.
  - `cargo build -p moonpool-transport`
  - Commit: `refactor(transport): Rc->Arc in rpc primitives`
- [ ] **4.2 Promises / futures**: `rpc/reply_promise.rs:7, 64`,
  `rpc/reply_future.rs:10`.
  - Commit: `refactor(transport): Rc->Arc in reply promise/future`
- [ ] **4.3 Queues / monitors**: `rpc/net_notified_queue.rs:8`,
  `rpc/failure_monitor.rs:13, 59, 92`.
  - Commit: `refactor(transport): Rc->Arc in queues/monitors`
- [ ] **4.4 Endpoint / stream layer**: `rpc/service_endpoint.rs:8`,
  `rpc/request_stream.rs:26, 51, 54`, `rpc/transport_handle.rs:9`.
  - Commit: `refactor(transport): Rc->Arc in endpoint/stream layer`
- [ ] **4.5 Transport orchestrator**:
  `rpc/delivery.rs:8, 85, 135, 149`,
  `rpc/net_transport.rs:50, 54, 132, 149, 157`.
  - Note: `delivery.rs` has 3 `spawn_local` sites (lines 85, 135, 149) —
    convert to `spawn`. Captures must be Send.
  - `failure_monitor.rs:59, 92` — 2 more `spawn_local` sites; same
    treatment.
  - Commit: `refactor(transport): Rc->Arc in net_transport/delivery, spawn_local->spawn`
- [ ] **4.6 Peer**: `peer/core.rs:21, 224, 227, 273`.
  - Commit: `refactor(transport): Rc->Arc in peer/core`
- [ ] **4.7 `#[service]` proc-macro**: in
  `moonpool-transport-derive/src/lib.rs:192`, change
  `#[async_trait::async_trait(?Send)]` to `#[async_trait::async_trait]` on
  the emitted handler trait. Decide whether the emitted trait needs a
  `Send + Sync + 'static` supertrait by checking how moonpool-transport uses
  it (search for `dyn .*Handler` or generic bounds on the handler type). If
  stored as `Box<dyn …>`, add the supertrait; if always generic, leave it
  off (impls supply their own bounds).
  - Commit: `refactor(transport-derive): drop ?Send from #[service] handler trait`
- [ ] **4.8 Tests**: `rpc/test_support.rs:7`, `tests/rpc_integration.rs`,
  any other transport tests under `moonpool-transport/tests/`.
  - Commit: `test(transport): migrate to Arc-based handles`
- [ ] Phase-end compile:
  `nix develop --command cargo build -p moonpool-transport && nix develop --command cargo nextest run -p moonpool-transport`

### Phase 5 — moonpool-explorer mmap newtype
Scope: wrap raw `*mut` mmap pointers in a Send-safe newtype.

- [ ] Add `moonpool-explorer/src/shared_ptr.rs`:
  ```rust
  // SAFETY: the wrapped pointer references memory allocated via
  // mmap(MAP_SHARED | MAP_ANONYMOUS). The mapping is intentionally shared
  // across threads and across forked processes; the pointer outlives all
  // accesses (mapping is freed only on explorer cleanup). Concurrent reads
  // and writes through this pointer go through whatever synchronization the
  // shared structure provides (atomics for SharedStats, etc.).
  #[repr(transparent)]
  pub struct SharedPtr<T: ?Sized>(pub *mut T);

  unsafe impl<T: ?Sized> Send for SharedPtr<T> {}
  unsafe impl<T: ?Sized> Sync for SharedPtr<T> {}

  impl<T: ?Sized> Clone for SharedPtr<T> { fn clone(&self) -> Self { SharedPtr(self.0) } }
  impl<T: ?Sized> Copy for SharedPtr<T> {}
  ```
  Re-export from `moonpool-explorer/src/lib.rs`.
- [ ] `moonpool-explorer/src/context.rs:26-50` — replace the 9 raw `*mut`
  fields (`Cell<*mut SharedStats>`, etc.) with
  `Cell<SharedPtr<…>>`. Update every read/write of `.get()` / `.set()`
  accordingly.
- [ ] `moonpool-explorer/src/sancov.rs:296-304` — replace the 3 raw `*mut`
  fields the same way.
- [ ] Compile per-crate:
  `nix develop --command cargo build -p moonpool-explorer && nix develop --command cargo nextest run -p moonpool-explorer`
- [ ] Commit: `refactor(explorer): wrap mmap pointers in SharedPtr newtype`

### Phase 6 — Tests, examples, final validation
- [ ] Migrate user-side `Process`/`Workload` impls to drop `?Send` (no
  `#[async_trait(?Send)]` left). For each: swap any `Rc/RefCell/Cell` to
  `Arc/RwLock/Atomic`. Files:
  - `moonpool-sim/tests/reboot.rs` — 10 impls at lines 23, 65, 108, 150, 176,
    241, 282, 327, 361, 406
  - `moonpool-sim/tests/hyper_http.rs:81, 118`
  - `moonpool-sim/tests/coverage_plateau.rs:20, 38`
  - `moonpool-sim/tests/exploration/tests.rs:25, 40, 62, 83, 101, 135`
- [ ] Commit: `test(sim): migrate Process/Workload impls to Send bounds`
- [ ] `moonpool-sim-examples/src/axum_web.rs:251` — the `spawn_local` capture
  holds a `SimTcpStream`. With Phase 3 done, `SimTcpStream` is Send. Convert
  to `tokio::spawn`. If hyper's API in this path requires `!Send` somewhere,
  isolate and document; otherwise full migrate.
- [ ] Commit: `refactor(examples): migrate axum_web to Send-bounded spawning`
- [ ] Update CLAUDE.md: remove the `Networking: #[async_trait(?Send)]` line
  from "Core Constraints"; add a note that traits are Send-bounded and the
  sim runtime uses `new_current_thread().build()`. Also update the "No
  LocalSet usage" line — that's now stricter (no `spawn_local` at all).
- [ ] Update relevant chapters in `book/src/`. Search for `?Send` and
  `spawn_local` and revise. Build the book:
  `nix develop --command mdbook build book/`
- [ ] Commit: `docs: update for Send-bounded traits`
- [ ] Workspace-wide checks:
  ```
  nix develop --command cargo fmt
  nix develop --command cargo clippy -- -D warnings
  nix develop --command cargo nextest run
  ```
- [ ] Determinism verification: run
  `nix develop --command cargo xtask sim run-all` and compare seed traces
  against the Phase 0 baseline. Must be byte-identical (or, if some test
  output includes timestamps/PIDs, identical modulo those known fields).
  Any divergence is a stop-the-line bug — investigate before merging.
- [ ] Perf spot-check: pick one RPC-heavy chaos test (e.g. `bit_flip` or
  `clock_drift`) and time it before/after. Target <10% slowdown from
  `Arc<RwLock>` overhead.
- [ ] `assert_sometimes!` coverage: compare hit set vs baseline. Coverage
  should not regress.
- [ ] Open the PR (manual; the user will trigger when ready). Title:
  `refactor: migrate moonpool to Send-bounded traits`.

## Known risk areas

- **NLL → RwLock friction** in `moonpool-sim/src/sim/world.rs` orchestration
  loop. The CLAUDE.md note documents the existing pattern: *"`conn` from
  `inner.network.connections.get_mut()` can coexist with
  `inner.event_queue.schedule()` (disjoint fields)"*. That pattern only works
  with `RefCell` + NLL. With `RwLock` you'll need a write guard that covers
  both, or extract `conn` data into locals, drop the guard, then re-acquire.
  Plan to refactor this hot path carefully — small mistakes here are perf
  cliffs (constant lock thrashing).
- **`SimTcpStream` Send-ness**: hyper / axum API surface. If `SimTcpStream`
  ends up `Send` (Phase 3 swap puts its inner state behind `Arc<RwLock>`),
  the `spawn_local` in `axum_web.rs:251` becomes `spawn`. Verify hyper's
  service trait accepts a Send stream in this version of the dep.
- **`tokio::sync::RwLock` for cross-await guards**: `std::sync::RwLock`
  guards must **not** be held across `.await`. If a site genuinely needs to
  hold a guard across an await (rare), switch that one site to
  `tokio::sync::RwLock` and document why.
- **`tokio_unstable` removal**: once `build_local()` is gone, audit CI for any
  `--cfg tokio_unstable` flag and remove it. Check `nix` flake, `xtask`
  Cargo args, and `.cargo/config.toml`.
- **Lock poisoning under sim panics**: when a test panic occurs inside a
  borrow, the `RwLock` becomes poisoned and subsequent `.expect(…)` calls
  panic too. That's intentional (a poisoned lock means broken invariants),
  but if the orchestrator is catching panics and continuing, this changes
  behavior — verify.

## Final verification checklist

- [ ] `cargo fmt` clean
- [ ] `cargo clippy -- -D warnings` clean
- [ ] `cargo nextest run` (full workspace) green
- [ ] `cargo xtask sim run-all` traces byte-identical to Phase 0 baseline
- [ ] `assert_sometimes!` coverage unchanged
- [ ] Perf <10% regression on one RPC-heavy chaos seed
- [ ] CLAUDE.md and `book/src/` updated
- [ ] No `?Send` left in the workspace:
  `rg "async_trait\(\?Send\)" -g '!target/'` returns nothing
- [ ] No `spawn_local` left in non-test, non-example code:
  `rg "spawn_local" -g '!target/' -g '!*/tests/*' -g '!*/examples/*'`
  returns nothing
- [ ] No `build_local` left:
  `rg "build_local" -g '!target/'` returns nothing
- [ ] No `tokio_unstable` cfg left:
  `rg "tokio_unstable" -g '!target/'` returns nothing
- [ ] `async-trait` dep removed from `moonpool-core/Cargo.toml`:
  `rg "^async-trait" moonpool-core/Cargo.toml` returns nothing.
  (`async-trait` remains in `moonpool-sim` and `moonpool-transport-derive`
  for the dyn-stored traits — that's intentional.)
- [ ] No `#[async_trait]` left in moonpool-core source:
  `rg "async_trait" moonpool-core/src/` returns nothing.

## When this TODO.md is done

- Delete this file (`git rm TODO.md`) in a final commit:
  `git commit --no-gpg-sign -m "chore: remove migration TODO (work complete)"`
- The branch is ready for PR review.
