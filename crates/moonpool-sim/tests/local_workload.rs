//! Local (non-`Send`) code under simulation.
//!
//! Code written for a thread-per-core runtime is generic over the local
//! provider traits and holds `Rc` and `RefCell` across awaits. The simulator
//! runs it unchanged: its backends implement the local traits through the
//! blanket impls, and `SimTaskProvider` spawns `!Send` tasks on the
//! deterministic executor. These tests pin that: the sim types implement the
//! local traits, a local process runs, dies with its crash and comes back on
//! restart, and the same seed replays the local tasks' interleaving exactly.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    FaultContext, FaultInjector, LocalNetworkProvider, LocalProviders, LocalStorageFile,
    LocalStorageProvider, LocalTaskProvider, LocalTcpListener, LocalTimeProvider, NetworkProvider,
    OpenOptions, Process, SimContext, SimNetworkProvider, SimProviders, SimStorageProvider,
    SimTaskProvider, SimTimeProvider, SimulationBuilder, SimulationError, SimulationResult,
    StateHandle, StorageProvider, Workload, assert_always,
};

fn implements_local_storage<S: LocalStorageProvider>() {}
fn implements_local_file<F: LocalStorageFile>() {}
fn implements_local_network<N: LocalNetworkProvider>() {}
fn implements_local_listener<L: LocalTcpListener>() {}
fn implements_local_time<T: LocalTimeProvider>() {}
fn implements_local_tasks<T: LocalTaskProvider>() {}
fn implements_local_bundle<P: LocalProviders>() {}
fn assert_send<T: Send>(_: &T) {}

#[test]
fn sim_backends_implement_the_local_traits() {
    implements_local_storage::<SimStorageProvider>();
    implements_local_file::<<SimStorageProvider as StorageProvider>::File>();
    implements_local_network::<SimNetworkProvider>();
    implements_local_listener::<<SimNetworkProvider as NetworkProvider>::TcpListener>();
    implements_local_time::<SimTimeProvider>();
    implements_local_tasks::<SimTaskProvider>();
    implements_local_bundle::<SimProviders>();
}

/// One tick of a local node: sleep, persist the counter, and bump it from a
/// child local task. The counter lives in an `Rc<RefCell<..>>` held across
/// every await, so none of these futures is `Send` unless the caller's is.
async fn tick<P: LocalProviders, T: LocalTaskProvider>(
    providers: &P,
    tasks: &T,
    counter: &Rc<RefCell<u64>>,
) -> std::io::Result<u64> {
    providers
        .time()
        .sleep(Duration::from_millis(10))
        .await
        .map_err(std::io::Error::other)?;
    let file = providers
        .storage()
        .open("local-node.bin", OpenOptions::create_write().read(true))
        .await?;
    let value = *counter.borrow();
    file.write_at(0, &value.to_le_bytes()).await?;
    file.sync_data().await?;

    let child = Rc::clone(counter);
    tasks
        .spawn_local_task("local-bump", async move {
            *child.borrow_mut() += 1;
        })
        .await
        .map_err(std::io::Error::other)?;
    Ok(*counter.borrow())
}

/// Generic local code instantiated with the sim's `Send` bundle (and a
/// `Send` counter) is a `Send` future; with an `Rc` counter it is not, and
/// still runs, as the process below shows.
#[test]
fn generic_local_code_on_sim_backends_is_send() {
    async fn sleep_then_now<P: LocalProviders>(providers: &P) -> Duration {
        let _ = providers.time().sleep(Duration::from_millis(1)).await;
        providers.time().now()
    }
    fn check(providers: &SimProviders) {
        assert_send(&sleep_then_now(providers));
    }
    let _ = check;
}

/// The node: a local task that ticks forever and publishes its counter.
async fn local_node(
    providers: SimProviders,
    tasks: SimTaskProvider,
    state: StateHandle,
    key: String,
) {
    let counter = Rc::new(RefCell::new(0_u64));
    loop {
        match tick(&providers, &tasks, &counter).await {
            Ok(value) => {
                record(format!(
                    "{key} {value} t={:?}",
                    LocalTimeProvider::now(providers.time())
                ));
                state.publish(&key, value);
            }
            Err(_) => return,
        }
    }
}

/// Spawns the local node as a `!Send` task, detaches it, and parks.
struct LocalProcess;

#[async_trait]
impl Process for LocalProcess {
    fn name(&self) -> &'static str {
        "local_process"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let key = format!("local_ticks:{}", ctx.my_ip());
        LocalTaskProvider::spawn_local_task(
            ctx.task(),
            "local-node",
            local_node(
                ctx.providers().clone(),
                ctx.task().clone(),
                ctx.state().clone(),
                key,
            ),
        )
        .detach();
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

/// Keeps the run alive long enough for the script to play out.
struct PatientWorkload;

#[async_trait]
impl Workload for PatientWorkload {
    fn name(&self) -> &'static str {
        "patient"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // A workload can drive local code too: `run_local` runs it as a
        // local task and hands its answer back, so the workload's own
        // future stays `Send` while the task holds an `Rc`.
        let rounds = ctx
            .run_local("local-observer", |ctx| async move {
                let inner = Rc::new(RefCell::new(Vec::new()));
                for _ in 0..10 {
                    let _ = LocalTimeProvider::sleep(ctx.time(), Duration::from_millis(50)).await;
                    inner.borrow_mut().push(LocalTimeProvider::now(ctx.time()));
                }
                let rounds = inner.borrow().len();
                rounds
            })
            .await;
        assert_always!(rounds == 10, "the local observer ran every round");
        for _ in 0..100 {
            moonpool_sim::TimeProvider::sleep(ctx.time(), Duration::from_millis(5))
                .await
                .map_err(|e| SimulationError::InvalidState(format!("sleep failed: {e}")))?;
        }
        Ok(())
    }
}

/// Crash process 0, hold it down, and require its local node to have
/// stopped; then restart it and require it to tick again.
struct CrashAndWatchLocalNode;

impl CrashAndWatchLocalNode {
    async fn pause(ctx: &FaultContext, ms: u64) -> SimulationResult<bool> {
        if ctx.chaos_shutdown().is_cancelled() {
            return Ok(false);
        }
        moonpool_sim::TimeProvider::sleep(ctx.time(), Duration::from_millis(ms))
            .await
            .map_err(|e| SimulationError::InvalidState(format!("sleep failed: {e}")))?;
        Ok(!ctx.chaos_shutdown().is_cancelled())
    }
}

#[async_trait]
impl FaultInjector for CrashAndWatchLocalNode {
    fn name(&self) -> &'static str {
        "crash_and_watch_local_node"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        if !Self::pause(ctx, 100).await? {
            return Ok(());
        }
        let target = ctx.process_ips()[0].clone();
        let key = format!("local_ticks:{target}");
        assert_always!(
            ctx.state().get::<u64>(&key).unwrap_or(0) > 0,
            "the local node ticked before the crash"
        );

        ctx.crash(&target)?;
        if !Self::pause(ctx, 30).await? {
            return Ok(());
        }
        let held: u64 = ctx.state().get(&key).unwrap_or(0);
        if !Self::pause(ctx, 200).await? {
            return Ok(());
        }
        assert_always!(
            ctx.state().get::<u64>(&key).unwrap_or(0) == held,
            "a crashed process's local tasks must stop with it"
        );

        ctx.restart(&target)?;
        // The restarted node starts its counter from zero again.
        let mut recovered = false;
        for _ in 0..20 {
            if !Self::pause(ctx, 20).await? {
                break;
            }
            if ctx.state().get::<u64>(&key).unwrap_or(held) < held {
                recovered = true;
                break;
            }
        }
        assert_always!(recovered, "the restarted process's local node ticks again");
        Ok(())
    }
}

fn run_local_simulation(seeds: Vec<u64>) -> moonpool_sim::SimulationReport {
    let iterations = seeds.len();
    SimulationBuilder::new()
        .processes(1, || Box::new(LocalProcess))
        .workload_factory(|| Box::new(PatientWorkload))
        .fault_factory(|| Box::new(CrashAndWatchLocalNode))
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(iterations)
        .set_debug_seeds(seeds)
        .run()
        .expect("simulation configuration is valid")
}

#[test]
fn a_local_process_runs_dies_with_its_crash_and_restarts() {
    let report = run_local_simulation(vec![7, 11, 13]);
    assert_eq!(report.iterations, 3);
    assert_eq!(
        report.failed_runs, 0,
        "the local node must behave like any task"
    );
    assert_eq!(report.successful_runs, 3);
}

thread_local! {
    /// Execution trace of the local nodes. Thread-local: each test runs its
    /// simulations on its own thread, and the executor never leaves it.
    static TRACE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn record(entry: String) {
    TRACE.with(|trace| trace.borrow_mut().push(entry));
}

fn take_trace() -> Vec<String> {
    TRACE.with(|trace| std::mem::take(&mut *trace.borrow_mut()))
}

#[test]
fn the_same_seed_replays_local_tasks_exactly() {
    take_trace();
    let first = run_local_simulation(vec![42]);
    let first_trace = take_trace();
    let second = run_local_simulation(vec![42]);
    let second_trace = take_trace();

    assert_eq!(first.failed_runs, 0);
    assert_eq!(second.failed_runs, 0);
    assert!(!first_trace.is_empty(), "the local node recorded its ticks");
    assert_eq!(first_trace, second_trace, "a local run must replay exactly");
}
