//! The moonpool deterministic executor: a single-threaded, seeded-random
//! async task scheduler purpose-built for simulation.
//!
//! # Why moonpool owns an executor
//!
//! The simulation used to run on a tokio current-thread runtime whose only
//! deterministic-scheduling lever was the unstable `RngSeed` API, forcing
//! `--cfg tokio_unstable` onto every downstream consumer (issue #151). The
//! deeper finding behind this module: tokio's current-thread scheduler is a
//! plain FIFO queue, so its seed never randomized task *scheduling* at all.
//! Owning the executor removes the unstable flag AND unlocks something tokio
//! cannot offer: task polling order that is a deterministic function of the
//! simulation seed, so the seed space explores task interleavings.
//!
//! # Why there is no reactor
//!
//! A production runtime pairs its executor with a reactor: an epoll/kqueue
//! loop that turns OS events into waker calls, plus a timer wheel. In the
//! simulation none of that exists; [`SimWorld`](crate::SimWorld)'s event
//! queue *is* the reactor. Virtual time, network delivery, storage
//! completion: every external event is a queue entry whose processing wakes
//! the parked task through its ordinary [`Waker`]. The executor therefore
//! only needs to answer one question: "of the tasks that are runnable right
//! now, which do I poll next?"
//!
//! # Scheduling: randomized AND deterministic
//!
//! Those two words are not in tension. Deterministic means "same seed, same
//! execution, bit for bit", not "one fixed schedule". The ready queue is a
//! `Vec` and the next task is chosen by `swap_remove` at a seeded-random
//! index (madsim uses the same distribution). The RNG is a private ChaCha8
//! stream derived from the iteration seed with [`EXEC_RNG_SALT`]; it is
//! deliberately NOT the counted `SIM_RNG` stream, so scheduling decisions
//! never perturb fork-explorer breakpoint replay (`count@seed` timelines).
//!
//! The payoff: when one simulation event wakes two tasks, FIFO would run
//! them in registration order on every seed forever, structurally hiding
//! any race between them. Here, which task runs first is a per-seed choice:
//! each seed replays exactly, and the *population* of seeds covers both
//! orders. Ordering bugs live exactly there.
//!
//! # The driver contract: `block_on` + `until_stalled`
//!
//! [`Executor::block_on`] pins the main future (the simulation orchestrator)
//! on the stack. It is not in the ready queue and is never scheduled
//! randomly; it is the *driver*, alternating with the task pool:
//!
//! ```text
//! loop {
//!     poll driver ─ Ready? ──────────────► return
//!     run_until_stalled()   // poll ready tasks in seeded-random order
//!                           // until no task is runnable
//!     (driver not woken AND queue empty) ─► panic: genuine deadlock
//! }
//! ```
//!
//! The orchestrator's step loops await [`until_stalled()`] between
//! `sim.step()` calls. Because the driver is only re-polled after a full
//! drain, resuming from `until_stalled().await` guarantees every task that
//! was runnable has been polled to `Pending` or completion. This is strictly
//! stronger than the contract the old tokio version relied on (`yield_now`
//! re-queuing the driver at the back of a FIFO), and it stays correct under
//! randomized scheduling, where "back of the queue" does not exist.
//!
//! Inside a task, use [`yield_now()`]: it reschedules the task at a seeded
//! random position. `until_stalled()` is driver-only (debug-asserted): if a
//! task awaited it, the drain-until-empty semantics would deadlock against
//! itself.
//!
//! # Task lifecycle, panics, and kill-on-drop
//!
//! Spawning goes through [`async_task`]: each task splits into a `Runnable`
//! (scheduled into the ready queue by its waker) and a
//! [`JoinHandle`] wrapping the `FallibleTask` (awaitable output). Three
//! contracts mirror tokio because the orchestrator depends on them:
//!
//! - **Detach on drop**: dropping a `JoinHandle` lets the task keep running.
//! - **Abort**: [`JoinHandle::abort`] cancels the task; awaiting the handle
//!   then yields `Err(JoinError::Cancelled)`.
//! - **Panic isolation**: every spawned future is wrapped in
//!   `catch_unwind`, so a panicking task never unwinds into the executor's
//!   run loop; its handle yields `Err(JoinError::Panicked)` and sibling
//!   tasks are unaffected.
//!
//! Dropping the executor kills every remaining task, replicating the
//! "dropping the per-iteration tokio runtime kills leaked tasks" contract
//! that isolates simulation iterations. The mechanism (borrowed from
//! async-executor): a registry keeps one waker per live task; `Drop` wakes
//! them all, which schedules every parked task into the ready queue, then
//! pops and drops `Runnable`s until the queue is empty. Dropping a
//! `Runnable` cancels its task and drops the future, and if that drop wakes
//! further tasks they land in the queue and are consumed by the same loop.
//!
//! # Fork safety
//!
//! The fork-based explorer ([`moonpool-explorer`]) forks the process while
//! tasks are being polled (assertion macros are fork points). Two rules keep
//! that sound: everything is single-threaded, and **the queue lock is never
//! held across `runnable.run()`**; wakes fired during a poll re-acquire the
//! lock, and a child process inherits no held locks.
//!
//! [`moonpool-explorer`]: https://docs.rs/moonpool-explorer

mod join;

pub use join::{JoinHandle, YieldNow, until_stalled, yield_now};

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use futures::FutureExt;
use parking_lot::Mutex;
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// Salt mixed into the iteration seed before seeding the executor's
/// scheduling RNG.
///
/// Decorrelates task-scheduling decisions from the in-run `SIM_RNG` stream
/// (and the other salted streams) while keeping them fully reproducible from
/// the same iteration seed. Like `CONFIG_RNG`/`SELECT_RNG`, this stream is
/// uncounted: scheduling never perturbs fork-explorer breakpoint replay.
const EXEC_RNG_SALT: u64 = 0x6578_6563_7363_6864; // "execschd"

/// Debug-build ceiling on polls within one `run_until_stalled` drain.
///
/// A task that unconditionally re-wakes itself forever (e.g. a busy
/// `yield_now` loop with no exit) would make drain-until-empty spin
/// endlessly; in debug builds we panic with the offending task's name
/// instead of hanging.
#[cfg(debug_assertions)]
const DRAIN_POLL_BOUND: u64 = 1_000_000;

/// Per-task metadata attached through `async_task::Builder::metadata`.
///
/// The `id` keys the kill-on-drop waker registry; the `name` makes the
/// per-poll trace events readable when debugging a seed.
#[derive(Debug)]
pub(crate) struct TaskMeta {
    /// Registry key, unique per spawn within one executor.
    pub(crate) id: u64,
    /// Human-readable task name (from `spawn`'s `name` argument).
    pub(crate) name: Arc<str>,
}

/// A schedulable task holding our metadata.
pub(crate) type ExecRunnable = async_task::Runnable<TaskMeta>;

thread_local! {
    /// The executor currently driving this thread (installed by `block_on`).
    static CURRENT: RefCell<Option<Handle>> = const { RefCell::new(None) };

    /// True while a task is being polled by `run_until_stalled` (as opposed
    /// to the driver). Guards the driver-only `until_stalled()` primitive.
    static IN_TASK: Cell<bool> = const { Cell::new(false) };
}

/// Report whether the current code is running inside a task poll (true) or
/// in the driver (false).
pub(crate) fn in_task() -> bool {
    IN_TASK.with(Cell::get)
}

/// State shared between the executor, its spawn handles, and task wakers.
struct Shared {
    /// The ready queue. `Vec` (not `VecDeque`) because the next task is
    /// chosen by seeded `swap_remove`, not FIFO order.
    queue: Mutex<Vec<ExecRunnable>>,
    /// One waker per live (spawned, not yet completed/cancelled) task,
    /// keyed by `TaskMeta::id`; consumed by kill-on-drop.
    wakers: Mutex<HashMap<u64, Waker>>,
    /// Next task id.
    next_id: AtomicU64,
}

/// Removes a task's waker from the registry when its future is dropped
/// (completion, cancellation, or executor drop).
struct Unregister {
    id: u64,
    shared: Arc<Shared>,
}

impl Drop for Unregister {
    fn drop(&mut self) {
        self.shared.wakers.lock().remove(&self.id);
    }
}

/// Wakes the driver by setting a flag `block_on` checks between drains.
struct DriverWaker {
    woken: AtomicBool,
}

impl Wake for DriverWaker {
    fn wake(self: Arc<Self>) {
        self.woken.store(true, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::Relaxed);
    }
}

/// Spawning access to a live executor (cloneable, thread-local installed).
#[derive(Clone)]
struct Handle {
    shared: Arc<Shared>,
}

impl Handle {
    fn spawn<T, F>(&self, name: &str, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let meta = TaskMeta {
            id,
            name: Arc::from(name),
        };

        // catch_unwind: a panicking task must resolve its JoinHandle with
        // JoinError::Panicked instead of unwinding into the run loop.
        let caught = AssertUnwindSafe(future).catch_unwind();
        // The guard unregisters the task's waker whenever the future is
        // dropped, keeping the kill-on-drop registry bounded by live tasks.
        let guard_shared = Arc::clone(&self.shared);
        let wrapped = async move {
            let _unregister = Unregister {
                id,
                shared: guard_shared,
            };
            caught.await
        };

        let schedule_shared = Arc::clone(&self.shared);
        let schedule = move |runnable: ExecRunnable| {
            schedule_shared.queue.lock().push(runnable);
        };

        let (runnable, task) = async_task::Builder::new()
            .metadata(meta)
            .spawn(move |_| wrapped, schedule);

        self.shared.wakers.lock().insert(id, runnable.waker());
        runnable.schedule();

        JoinHandle::new(task.fallible())
    }
}

/// Installs a [`Handle`] as the thread's current executor for the duration
/// of a `block_on` call.
struct CurrentGuard;

impl CurrentGuard {
    fn install(handle: Handle) -> Self {
        CURRENT.with(|current| {
            let mut current = current.borrow_mut();
            assert!(
                current.is_none(),
                "Executor::block_on is not reentrant: an executor is already running on this thread"
            );
            *current = Some(handle);
        });
        Self
    }
}

impl Drop for CurrentGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| current.borrow_mut().take());
    }
}

/// Spawn a named task onto the executor currently running on this thread.
///
/// The returned [`JoinHandle`] mirrors tokio semantics: awaiting it yields
/// the task's output (or a [`JoinError`](moonpool_core::JoinError) if the
/// task was aborted or panicked), dropping it detaches the task, and
/// [`JoinHandle::abort`] cancels it.
///
/// # Panics
///
/// Panics when called outside [`Executor::block_on`], mirroring
/// `tokio::spawn` outside a runtime.
pub fn spawn<T, F>(name: &str, future: F) -> JoinHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .expect("executor::spawn called outside Executor::block_on")
            .spawn(name, future)
    })
}

/// The deterministic single-threaded executor.
///
/// One instance is created per simulation iteration, seeded from the
/// iteration seed; dropping it cancels every task that is still alive (see
/// the [module docs](self) for the kill-on-drop mechanics).
///
/// # Examples
///
/// ```
/// let mut executor = moonpool_sim::executor::Executor::new(42);
/// let sum = executor.block_on(async {
///     let task = moonpool_sim::executor::spawn("adder", async { 1 + 2 });
///     task.await.expect("task completed")
/// });
/// assert_eq!(sum, 3);
/// ```
pub struct Executor {
    shared: Arc<Shared>,
    /// Seeded scheduling RNG (uncounted stream, see [`EXEC_RNG_SALT`]).
    rng: ChaCha8Rng,
    /// The iteration seed, kept for stall diagnostics.
    seed: u64,
}

impl Executor {
    /// Create an executor whose scheduling decisions derive from `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            shared: Arc::new(Shared {
                queue: Mutex::new(Vec::new()),
                wakers: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(0),
            }),
            rng: ChaCha8Rng::seed_from_u64(seed ^ EXEC_RNG_SALT),
            seed,
        }
    }

    /// Run `main` as the driver future to completion, interleaving it with
    /// the task pool (see the [module docs](self) for the drive loop).
    ///
    /// `main` needs no `Send` bound: it is pinned on this stack and never
    /// enters the ready queue.
    ///
    /// # Panics
    ///
    /// - if called while another `block_on` is running on this thread;
    /// - on a genuine deadlock: the driver is not woken, returns `Pending`,
    ///   and no task is runnable (with the seed in the message, replacing
    ///   tokio's silent forever-park).
    pub fn block_on<F: Future>(&mut self, main: F) -> F::Output {
        let _guard = CurrentGuard::install(Handle {
            shared: Arc::clone(&self.shared),
        });

        let mut main = std::pin::pin!(main);
        let driver_waker = Arc::new(DriverWaker {
            woken: AtomicBool::new(false),
        });
        let waker = Waker::from(Arc::clone(&driver_waker));
        let mut cx = Context::from_waker(&waker);

        loop {
            driver_waker.woken.store(false, Ordering::Relaxed);
            if let Poll::Ready(output) = main.as_mut().poll(&mut cx) {
                return output;
            }

            self.run_until_stalled();

            assert!(
                driver_waker.woken.load(Ordering::Relaxed) || !self.shared.queue.lock().is_empty(),
                "deterministic executor stalled (seed {}): the driver future is not \
                 woken and no task is runnable, so every remaining future awaits a \
                 wake that can never arrive",
                self.seed
            );
        }
    }

    /// Poll ready tasks in seeded-random order until none is runnable.
    ///
    /// The queue lock is released around every `runnable.run()` (fork
    /// safety; wakes fired during a poll re-acquire it to enqueue).
    fn run_until_stalled(&mut self) {
        #[cfg(debug_assertions)]
        let mut polls: u64 = 0;

        loop {
            let runnable = {
                let mut queue = self.shared.queue.lock();
                if queue.is_empty() {
                    break;
                }
                let index = self.rng.random_range(0..queue.len());
                queue.swap_remove(index)
            };

            #[cfg(debug_assertions)]
            {
                polls += 1;
                assert!(
                    polls < DRAIN_POLL_BOUND,
                    "executor drain exceeded {} polls (seed {}): task '{}' appears to \
                     busy-yield forever without an external wake",
                    DRAIN_POLL_BOUND,
                    self.seed,
                    runnable.metadata().name,
                );
            }

            tracing::trace!(
                task_id = runnable.metadata().id,
                task = %runnable.metadata().name,
                "executor poll"
            );

            IN_TASK.with(|flag| flag.set(true));
            runnable.run();
            IN_TASK.with(|flag| flag.set(false));
        }
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        // Wake every live task: completed tasks ignore it, parked tasks get
        // their Runnable scheduled into the queue.
        let wakers: Vec<Waker> = self
            .shared
            .wakers
            .lock()
            .drain()
            .map(|(_, waker)| waker)
            .collect();
        for waker in wakers {
            waker.wake();
        }

        // Drop Runnables until the queue is empty: dropping one cancels its
        // task and drops the future; if that wakes further tasks they are
        // scheduled into the queue and consumed by this same loop.
        loop {
            let runnable = self.shared.queue.lock().pop();
            match runnable {
                Some(runnable) => drop(runnable),
                None => break,
            }
        }
    }
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor")
            .field("seed", &self.seed)
            .field("ready_tasks", &self.shared.queue.lock().len())
            .field("live_tasks", &self.shared.wakers.lock().len())
            .finish_non_exhaustive()
    }
}
