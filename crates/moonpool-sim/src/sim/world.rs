//! Global deterministic scheduler and lifecycle coordination.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak},
    task::Waker,
    time::Duration,
};

use tracing::instrument;

use crate::{
    SimulationError, SimulationResult,
    chaos::fault_events::SimFaultEvent,
    network::{
        NetworkConfiguration,
        sim::{
            NetworkActions, NetworkEvent, NetworkOperationId, NetworkSimulation, SimNetworkProvider,
        },
    },
};

use super::{
    cpu::CpuState,
    events::{Event, ScheduleError, ScheduleId, Scheduler},
    rng::{reset_sim_rng, set_sim_seed, sim_random},
    sleep::SleepFuture,
    wakers::{WakeBatch, Wakers},
};
use crate::storage::sim::{OperationId, StorageEngine};
use moonpool_core::SystemSample;

/// Poison-checking lock boundary shared by simulation handles.
#[derive(Debug)]
pub(crate) struct WorldLock<T>(RwLock<T>);

impl<T> WorldLock<T> {
    fn new(value: T) -> Self {
        Self(RwLock::new(value))
    }

    pub(crate) fn read(&self) -> RwLockReadGuard<'_, T> {
        self.0.read().expect("RwLock poisoned: prior task panicked")
    }

    pub(crate) fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.0
            .write()
            .expect("RwLock poisoned: prior task panicked")
    }
}

/// How many system samples [`SimWorld::system_samples`] keeps per process.
pub const SYSTEM_SAMPLES_KEPT: usize = 64;

/// State shared by the scheduler and the independent resource engines.
#[derive(Debug)]
pub(crate) struct SimInner {
    pub(crate) scheduler: Scheduler<Event>,
    pub(crate) timer_time: Duration,
    pub(crate) network: NetworkSimulation,
    pub(crate) network_schedules: BTreeMap<NetworkOperationId, ScheduleId>,
    pub(crate) storage: StorageEngine,
    pub(crate) storage_schedules: BTreeMap<OperationId, ScheduleId>,
    /// The last system samples handed to each process, newest last (see
    /// [`SimWorld::system_samples`]).
    pub(crate) system_samples: BTreeMap<IpAddr, std::collections::VecDeque<SystemSample>>,
    pub(crate) wakers: Wakers,
    timer_schedules: BTreeMap<u64, ScheduleId>,
    pub(crate) next_task_id: u64,
    pub(crate) awakened_tasks: BTreeSet<u64>,
    pub(crate) events_processed: u64,
    pub(crate) last_processed_event: Option<Event>,
    pub(crate) pending_faults: Vec<SimFaultRecord>,
    buggified_delay_window: BuggifiedDelayWindow,
    /// Set once [`SimWorld::enter_recovery_mode`] has run. The simulator
    /// injects no new faults from that point on.
    recovery_mode: bool,
    /// The CPU model: one slot per process, the parked runnables, the
    /// active poll's local clock (see `crate::sim::cpu`).
    pub(crate) cpu: CpuState,
}

/// Campaign lifetime for the global sleep-delay fault.
#[derive(Debug, Clone, Copy)]
enum BuggifiedDelayWindow {
    /// Raw `SimWorld` users retain the historical whole-run behavior.
    Unbounded,
    /// Campaign setup, a campaign without a chaos duration, or the quiet tail.
    Inactive,
    /// The run phase is inside its configured chaos interval.
    ActiveUntil(Duration),
}

impl SimInner {
    pub(crate) fn new_with_config(network_config: NetworkConfiguration) -> Self {
        Self {
            scheduler: Scheduler::new(),
            timer_time: Duration::ZERO,
            network: NetworkSimulation::new(network_config),
            network_schedules: BTreeMap::new(),
            storage: StorageEngine::default(),
            storage_schedules: BTreeMap::new(),
            wakers: Wakers::default(),
            timer_schedules: BTreeMap::new(),
            next_task_id: 0,
            awakened_tasks: BTreeSet::new(),
            events_processed: 0,
            last_processed_event: None,
            pending_faults: Vec::new(),
            system_samples: BTreeMap::new(),
            buggified_delay_window: BuggifiedDelayWindow::Unbounded,
            recovery_mode: false,
            cpu: CpuState::default(),
        }
    }

    /// The current time of the caller: the local clock of the process whose
    /// poll is active under the CPU model, else the scheduler's clock.
    pub(crate) fn now(&self) -> Duration {
        self.cpu.local().unwrap_or_else(|| self.scheduler.now())
    }

    /// Whether [`SimWorld::enter_recovery_mode`] has run.
    ///
    /// Every setter that installs a caller-supplied fault configuration has to
    /// consult this: the recovery boundary promises no new simulator faults for
    /// the rest of the run, and a configuration installed after it must not be
    /// able to re-arm one.
    pub(crate) fn recovery_mode(&self) -> bool {
        self.recovery_mode
    }

    pub(crate) fn schedule_after(&mut self, event: Event, delay: Duration) {
        let Some(time) = self.now().checked_add(delay) else {
            tracing::error!(error = %ScheduleError::TimeOverflow, "failed to schedule simulation event");
            return;
        };
        self.schedule_at(event, time);
    }

    pub(crate) fn schedule_at(&mut self, event: Event, time: Duration) {
        if let Err(error) = self.schedule_effect_at(time, event) {
            tracing::error!(%error, "failed to schedule simulation event");
        }
    }

    /// Schedule `event` at `time`: the one door every event goes through.
    ///
    /// While a process's poll is active under the CPU model, an event it
    /// causes is one of its effects, and an effect never lands before the
    /// CPU time of the poll that caused it: `time` is at or after the
    /// poll's local clock (debug-asserted).
    pub(crate) fn schedule_effect_at(
        &mut self,
        time: Duration,
        event: Event,
    ) -> Result<ScheduleId, ScheduleError> {
        if let Some(local) = self.cpu.local() {
            debug_assert!(
                time >= local,
                "cpu: an effect of a poll is stamped {time:?}, before the poll's local clock {local:?}: {event:?}"
            );
        }
        self.scheduler.schedule_at(time, event)
    }

    pub(crate) fn record_fault(&mut self, event: SimFaultEvent) {
        let time_ms = u64::try_from(self.now().as_millis()).unwrap_or(u64::MAX);
        self.pending_faults.push(SimFaultRecord { time_ms, event });
    }

    /// Apply the network engine's effects. The engine runs on the
    /// scheduler's clock; an event a process's poll causes is shifted by
    /// how far the poll's local clock is ahead (see `crate::sim::cpu`).
    pub(crate) fn apply_network(&mut self, actions: NetworkActions) {
        let offset = self.cpu_offset();
        let (scheduled, faults) = actions.into_parts();
        for (at, event) in scheduled {
            self.schedule_at(Event::Network(event), at.saturating_add(offset));
        }
        for fault in faults {
            self.record_fault(fault);
        }
    }
}

/// A fault stamped with the logical time at which it was injected.
#[derive(Debug, Clone)]
pub struct SimFaultRecord {
    /// Simulation time in milliseconds.
    pub time_ms: u64,
    /// Injected fault.
    pub event: SimFaultEvent,
}

/// The simulated world: one scheduler, one clock, and the network and
/// storage engines it drives.
///
/// `SimWorld` owns the global [`Scheduler<Event>`](Scheduler), which owns
/// logical time, same-time FIFO order and cancellation. [`step`](Self::step)
/// pops the next event, advances the clock to it and dispatches it to the
/// network engine, the storage engine or the process lifecycle. The engines
/// return wakes, and the world runs them only after it releases its lock.
///
/// The [`SimulationBuilder`](crate::SimulationBuilder) builds one world per
/// seed. Build one by hand only to drive providers directly in a test:
///
/// ```rust
/// use std::time::Duration;
/// use moonpool_sim::SimWorld;
///
/// let mut sim = SimWorld::new_with_seed(42);
/// let _sleep = sim.sleep(Duration::from_millis(10));
/// // The sleep scheduled a timer event; stepping fires it and moves time.
/// sim.run_until_empty();
/// assert_eq!(sim.current_time(), Duration::from_millis(10));
/// ```
#[derive(Debug)]
pub struct SimWorld {
    pub(crate) inner: Arc<WorldLock<SimInner>>,
}

impl SimWorld {
    fn create(network_config: NetworkConfiguration, seed: u64) -> Self {
        // A new world starts without a CPU model: the thread's gate, if an
        // older world left one, no longer applies.
        super::cpu::uninstall();
        reset_sim_rng();
        set_sim_seed(seed);
        crate::chaos::assertions::reset_assertion_results();
        Self {
            inner: Arc::new(WorldLock::new(SimInner::new_with_config(network_config))),
        }
    }

    /// Creates a simulation with default configuration and seed zero.
    #[must_use]
    pub fn new() -> Self {
        Self::create(NetworkConfiguration::default(), 0)
    }

    /// Creates a simulation with a deterministic seed.
    #[must_use]
    pub fn new_with_seed(seed: u64) -> Self {
        Self::create(NetworkConfiguration::default(), seed)
    }

    /// Creates a simulation with custom network configuration.
    #[must_use]
    pub fn new_with_network_config(config: NetworkConfiguration) -> Self {
        Self::create(config, 0)
    }

    /// Creates a simulation with custom network configuration and seed.
    #[must_use]
    #[instrument(level = "debug", skip(config))]
    pub fn new_with_network_config_and_seed(config: NetworkConfiguration, seed: u64) -> Self {
        Self::create(config, seed)
    }

    /// Processes one delayed event and returns whether another event is queued.
    #[instrument(level = "trace", skip(self))]
    pub fn step(&mut self) -> bool {
        let mut requeue = Vec::new();
        let (processed, wakes) = {
            let mut inner = self.inner.write();
            let Some(scheduled) = inner.scheduler.pop() else {
                inner.last_processed_event = None;
                return false;
            };
            let now = inner.now();
            let actions = inner.network.before_event(now);
            inner.apply_network(actions);
            let mut wakes = WakeBatch::default();

            let event = scheduled.into_value();
            inner.last_processed_event = Some(event.clone());
            inner.events_processed += 1;
            match event {
                Event::CpuFree { ip } => {
                    requeue = inner.cpu_on_free(ip);
                }
                Event::CpuWake { id } => {
                    wakes.append(inner.cpu_take_deferred(id));
                }
                Event::Timer { task_id } => {
                    inner.timer_schedules.remove(&task_id);
                    inner.awakened_tasks.insert(task_id);
                    wakes.push(inner.wakers.tasks.take(&task_id));
                }
                Event::Network(event) => {
                    if let NetworkEvent::OperationReady { operation_id } = &event {
                        inner.network_schedules.remove(operation_id);
                    }
                    let (actions, network_wakes) = inner.network.handle_event(event, now);
                    inner.apply_network(actions);
                    wakes.append(network_wakes);
                }
                Event::Storage(event) => {
                    inner.storage_schedules.remove(&event.operation_id());
                    super::storage_ops::handle_storage_event(&mut inner, event, &mut wakes);
                }
                Event::Shutdown => {
                    // Every cancelled timer resolves, whether or not its sleep
                    // was polled yet: a sleep constructed but not polled has a
                    // schedule and no waker, and would otherwise pend forever
                    // on a timer that no longer exists.
                    let timer_schedules = std::mem::take(&mut inner.timer_schedules);
                    for (task_id, schedule_id) in timer_schedules {
                        inner.scheduler.cancel(schedule_id);
                        inner.awakened_tasks.insert(task_id);
                    }
                    let task_wakers = inner.wakers.tasks.drain().collect::<Vec<_>>();
                    for (task_id, waker) in task_wakers {
                        inner.awakened_tasks.insert(task_id);
                        wakes.extend([waker]);
                    }
                    wakes.append(inner.network.shutdown_waiters());
                    let network_events = inner
                        .scheduler
                        .iter()
                        .filter(|scheduled| matches!(scheduled.value(), Event::Network(_)))
                        .map(super::events::Scheduled::id)
                        .collect::<Vec<_>>();
                    for schedule_id in network_events {
                        inner.scheduler.cancel(schedule_id);
                    }
                    let schedule_ids = std::mem::take(&mut inner.network_schedules);
                    for (operation_id, schedule_id) in schedule_ids {
                        inner.scheduler.cancel(schedule_id);
                        inner.network.fail_operation(operation_id);
                    }
                    let now = inner.now();
                    inner.storage.set_clock(now);
                    let storage_actions = inner.storage.shutdown();
                    wakes.append(super::storage_ops::apply_storage_actions(
                        &mut inner,
                        storage_actions,
                    ));
                }
                Event::ProcessRestart { .. }
                | Event::ProcessSpawn { .. }
                | Event::ProcessRetire { .. }
                | Event::ProcessGracefulShutdown { .. }
                | Event::ProcessForceKill { .. } => {}
            }
            (true, wakes)
        };
        wakes.wake();
        // A freed CPU slot: its parked runnables go back on the ready
        // queue, outside the lock like every wake.
        for runnable in requeue {
            runnable.schedule();
        }
        processed && !self.inner.read().scheduler.is_empty()
    }

    /// Processes queued workload events until stalled.
    #[instrument(level = "debug", skip(self))]
    pub fn run_until_empty(&mut self) {
        while self.step() {
            let inner = self.inner.read();
            if inner.events_processed.is_multiple_of(50)
                && inner
                    .scheduler
                    .iter()
                    .all(|event| event.value().is_infrastructure_event())
            {
                break;
            }
        }
    }

    /// Returns current logical time.
    #[must_use]
    pub fn current_time(&self) -> Duration {
        self.now()
    }

    /// Returns exact current logical time.
    #[must_use]
    pub fn now(&self) -> Duration {
        self.inner.read().now()
    }

    /// Returns a deterministically drifted timer reading.
    #[must_use]
    ///
    /// The drift is one clock for the whole world, kept on the scheduler's
    /// clock; a process whose poll is active under the CPU model reads it no
    /// lower than its own local clock. The local clock is never stored in
    /// the shared reading, so one process's CPU time never shows in another
    /// process's timer.
    pub fn timer(&self) -> Duration {
        let mut inner = self.inner.write();
        let now = inner.scheduler_now();
        let local = inner.now();
        let chaos = &inner.network.config().chaos;
        if !chaos.clock_drift_enabled {
            // Never hand back a reading below one already given out. Drift that
            // accumulated before it was switched off (recovery mode) is a real
            // skew the node still has to live with; rewinding the clock instead
            // would be a *new* fault, and a nastier one than the drift.
            inner.timer_time = inner.timer_time.max(now);
            return inner.timer_time.max(local);
        }
        let max_timer = now.saturating_add(chaos.clock_drift_max);
        if inner.timer_time < max_timer {
            let gap = max_timer.saturating_sub(inner.timer_time).as_secs_f64();
            inner.timer_time += Duration::from_secs_f64(sim_random::<f64>() * gap / 2.0);
        }
        inner.timer_time = inner.timer_time.max(now);
        inner.timer_time.max(local)
    }

    /// Schedules an event after a relative delay.
    #[instrument(level = "trace", skip(self))]
    pub fn schedule_event(&self, event: Event, delay: Duration) {
        self.inner.write().schedule_after(event, delay);
    }

    /// Schedules an event at an absolute logical time.
    #[instrument(level = "trace", skip(self))]
    pub fn schedule_event_at(&self, event: Event, time: Duration) {
        self.inner.write().schedule_at(event, time);
    }

    /// Returns a non-owning simulation handle.
    #[must_use]
    pub fn downgrade(&self) -> WeakSimWorld {
        WeakSimWorld {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Returns whether delayed events remain.
    #[must_use]
    pub fn has_pending_events(&self) -> bool {
        !self.inner.read().scheduler.is_empty()
    }

    /// Returns the number of delayed events.
    #[must_use]
    pub fn pending_event_count(&self) -> usize {
        self.inner.read().scheduler.len()
    }

    /// Returns and clears faults recorded since the previous drain.
    #[must_use]
    pub fn take_faults(&self) -> Vec<SimFaultRecord> {
        std::mem::take(&mut self.inner.write().pending_faults)
    }

    /// Returns the last event processed by [`step`](Self::step).
    #[must_use]
    pub fn last_processed_event(&self) -> Option<Event> {
        self.inner.read().last_processed_event.clone()
    }

    /// Returns simulation metrics.
    #[must_use]
    pub fn extract_metrics(&self) -> crate::runner::SimulationMetrics {
        let inner = self.inner.read();
        crate::runner::SimulationMetrics {
            wall_time: Duration::ZERO,
            simulated_time: inner.now(),
            events_processed: inner.events_processed,
            // Filled in by the runner, which owns the per-node metrics
            // sources; the engine has no view of application registries.
            app_metrics: Vec::new(),
            app_series: std::collections::BTreeMap::new(),
            dropped_metric_points: 0,
            deployment: crate::runner::membership::DeploymentChanges::default(),
        }
    }

    /// Creates a network provider scoped to an IP.
    #[must_use]
    pub fn network_provider(&self, ip: IpAddr) -> SimNetworkProvider {
        SimNetworkProvider::new(self.downgrade(), ip)
    }

    /// Creates a time provider.
    #[must_use]
    pub fn time_provider(&self) -> crate::providers::SimTimeProvider {
        crate::providers::SimTimeProvider::new(self.downgrade())
    }

    /// Creates a storage provider scoped to an IP.
    #[must_use]
    pub fn storage_provider(&self, ip: IpAddr) -> crate::storage::SimStorageProvider {
        crate::storage::SimStorageProvider::new(self.downgrade(), ip)
    }

    /// Replaces the default storage configuration.
    ///
    /// After [`enter_recovery_mode`](Self::enter_recovery_mode) the fault
    /// probabilities in `config` are stripped before it is installed, so the
    /// no-new-faults promise survives a later reconfiguration. Performance
    /// knobs (IOPS, bandwidth, latencies, throttle multipliers) are installed
    /// as given.
    #[instrument(level = "debug", skip_all)]
    pub fn set_storage_config(&mut self, mut config: crate::storage::StorageConfiguration) {
        let mut inner = self.inner.write();
        if inner.recovery_mode() {
            config.disable_fault_injection();
        }
        inner.storage.set_config(config);
    }

    /// Returns an active disk episode for one IP.
    #[must_use]
    pub fn disk_episode_for(
        &self,
        ip: IpAddr,
    ) -> Option<crate::storage::sim::DiskDegradationState> {
        self.inner.read().storage.disk_episode_for(ip)
    }

    /// The cumulative counters of `ip`'s simulated disk now.
    ///
    /// The same numbers the [`SimSystemProvider`](crate::SimSystemProvider)
    /// of a process on `ip` reports, so a harness can check what a process
    /// says about its own disk against the simulator's ground truth.
    #[must_use]
    pub fn disk_counters(&self, ip: IpAddr) -> moonpool_core::DiskCounters {
        let inner = self.inner.read();
        inner.storage.disk_counters(ip, inner.now())
    }

    /// Take one system sample for the process at `ip`, and remember it.
    ///
    /// The CPU counters come from the process's CPU slot (zero while the
    /// CPU model is off): the CPU time used up to the sample instant and
    /// never after it, so `process_cpu / at` stays at or below its one core.
    /// With one core, the machine's busy time and the run loop's are the
    /// process's.
    pub(crate) fn system_sample(&self, ip: IpAddr) -> SystemSample {
        let mut inner = self.inner.write();
        let at = inner.now();
        let busy = inner.cpu_busy(ip, at);
        let sample = SystemSample {
            at,
            process_cpu: busy,
            cores: 1,
            machine_busy: busy,
            machine_total: at,
            run_loop_busy: Some(busy),
            disk: Some(inner.storage.disk_counters(ip, at)),
            space: inner.storage.disk_space(ip),
        };
        let kept = inner.system_samples.entry(ip).or_default();
        if kept.len() == SYSTEM_SAMPLES_KEPT {
            kept.pop_front();
        }
        kept.push_back(sample.clone());
        sample
    }

    /// The last system samples the [`SimSystemProvider`](crate::SimSystemProvider)
    /// of `ip` handed out, oldest first, at most
    /// [`SYSTEM_SAMPLES_KEPT`].
    ///
    /// A harness checks a window a process reports against these: the
    /// simulator's own record of what the process read, never the process's
    /// word for it.
    #[must_use]
    pub fn system_samples(&self, ip: IpAddr) -> Vec<SystemSample> {
        self.inner
            .read()
            .system_samples
            .get(&ip)
            .map(|kept| kept.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Give `ip`'s disk a size limit, or lift it with `None` (see
    /// [`DiskCapacity`](crate::DiskCapacity)).
    ///
    /// From then on a write or `set_len` that grows one of its files past the
    /// limit fails with `ENOSPC` and lands nothing; deleting or shrinking a
    /// file frees space. The limit holds across a crash or a wipe of the
    /// process. In recovery mode the call does nothing:
    /// [`enter_recovery_mode`](Self::enter_recovery_mode) lifts every limit.
    /// Consumes no randomness.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic, or
    /// if the capacity's `amplification` is zero.
    pub fn set_disk_capacity(&self, ip: IpAddr, capacity: Option<crate::DiskCapacity>) {
        let mut inner = self.inner.write();
        if inner.recovery_mode {
            return;
        }
        inner.storage.set_disk_capacity(ip, capacity);
    }

    /// `ip`'s disk size limit, if it has one.
    #[must_use]
    pub fn disk_capacity(&self, ip: IpAddr) -> Option<crate::DiskCapacity> {
        self.inner.read().storage.disk_capacity(ip)
    }

    /// The space on `ip`'s disk now, when it has a size limit: what its
    /// [`SimSystemProvider`](crate::SimSystemProvider) samples report as
    /// [`SystemSample::space`].
    #[must_use]
    pub fn disk_space(&self, ip: IpAddr) -> Option<moonpool_core::DiskSpace> {
        self.inner.read().storage.disk_space(ip)
    }

    /// Whether `ip`'s disk has failed and parks every operation forever (see
    /// [`StorageConfiguration::disk_failure_probability`](crate::StorageConfiguration::disk_failure_probability)).
    #[must_use]
    pub fn is_disk_failed(&self, ip: IpAddr) -> bool {
        self.inner.read().storage.is_disk_failed(ip)
    }

    /// Creates a cancellable simulation sleep.
    #[must_use]
    pub fn sleep(&self, duration: Duration) -> SleepFuture {
        let duration = self.apply_buggified_delay(duration);
        let mut inner = self.inner.write();
        let task_id = inner.next_task_id;
        let Some(next) = task_id.checked_add(1) else {
            return SleepFuture::failed(
                self.downgrade(),
                "sleep task identifier space exhausted".to_string(),
            );
        };
        inner.next_task_id = next;
        let Some(at) = inner.now().checked_add(duration) else {
            return SleepFuture::failed(self.downgrade(), ScheduleError::TimeOverflow.to_string());
        };
        match inner.schedule_effect_at(at, Event::Timer { task_id }) {
            Ok(schedule_id) => {
                inner.timer_schedules.insert(task_id, schedule_id);
                SleepFuture::new(self.downgrade(), task_id, schedule_id)
            }
            Err(error) => SleepFuture::failed(self.downgrade(), error.to_string()),
        }
    }

    fn apply_buggified_delay(&self, duration: Duration) -> Duration {
        let inner = self.inner.read();
        let chaos = &inner.network.config().chaos;
        if !chaos.buggified_delay_enabled || chaos.buggified_delay_max == Duration::ZERO {
            return duration;
        }
        let inside_chaos_window = match inner.buggified_delay_window {
            BuggifiedDelayWindow::Unbounded => true,
            BuggifiedDelayWindow::Inactive => false,
            BuggifiedDelayWindow::ActiveUntil(deadline) => inner.now() < deadline,
        };
        if !inside_chaos_window {
            return duration;
        }
        if sim_random::<f64>() < chaos.buggified_delay_probability {
            duration.saturating_add(
                chaos
                    .buggified_delay_max
                    .mul_f64(sim_random::<f64>().powf(1000.0)),
            )
        } else {
            duration
        }
    }

    /// Keep campaign setup free of the global sleep-delay fault until the run
    /// phase establishes its chaos window.
    pub(crate) fn prepare_buggified_delay_campaign(&self) {
        self.inner.write().buggified_delay_window = BuggifiedDelayWindow::Inactive;
    }

    /// Activate the global sleep-delay fault until the configured chaos
    /// interval ends. `None` leaves it inactive because no chaos phase exists.
    pub(crate) fn start_buggified_delay_window(&self, duration: Option<Duration>) {
        let mut inner = self.inner.write();
        inner.buggified_delay_window = duration
            .map_or(BuggifiedDelayWindow::Inactive, |duration| {
                BuggifiedDelayWindow::ActiveUntil(inner.now().saturating_add(duration))
            });
    }

    /// End fault injection for the rest of the run: the simulator stops
    /// generating new faults and heals the environmental partitions it is
    /// holding, so the system under test gets a quiet tail to recover in.
    ///
    /// The runner calls this once, at the [`chaos_duration`] cutoff, together
    /// with cancelling the fault-injector shutdown token. Calling it directly
    /// is the raw-`SimWorld` equivalent.
    ///
    /// # What stops
    ///
    /// - network: partitions, clogs, bit flips, spontaneous closes, connect
    ///   failures, clock drift, buggified sleep delays, and new per-pair
    ///   latency degradation;
    /// - storage: read and write EIO, read- and write-time corruption,
    ///   misdirected and phantom writes, sync failures, short transfers,
    ///   unsynced directory-entry loss, lying syncs, and new disk stall or
    ///   throttle episodes.
    ///
    /// The crash *model* is deliberately not among them: how an unsynced
    /// sector resolves is the disk's physics, not an environment generating
    /// faults. Recovery mode stops the simulator from generating crashes; one
    /// that still happens resolves with the same physics it would have had
    /// before the cutoff.
    ///
    /// # What is healed
    ///
    /// Every partition currently in force — directed pair cuts and the
    /// asymmetric send-side and receive-side blocks alike. A partition is
    /// environmental: nothing in the system under test can clear it, so
    /// leaving one up would make the quiet tail untestable rather than quiet.
    ///
    /// Every gray failure ([`set_slowness`](Self::set_slowness)): each
    /// machine's CPU, disk and network factors go back to 1, so the quiet
    /// tail runs at full speed.
    ///
    /// # What survives
    ///
    /// Everything already done. Corrupted sectors stay corrupted, lost writes
    /// stay lost, misdirected and phantom writes are not undone, a connection
    /// the application already saw close stays closed, a black-holed
    /// connection (one whose sends are silently discarded) stays black-holed,
    /// a process already killed is not resurrected, and application state is
    /// left exactly as the chaos phase left it. A failed disk (see
    /// [`is_disk_failed`](Self::is_disk_failed)) is permanent for the rest of
    /// the run: it keeps parking every operation, and once the cutoff has
    /// passed nothing can crash or wipe it. Finite
    /// effects already started — a disk stall or throttle episode, a clog, a
    /// packet already scheduled with delay — keep their deadlines and expire
    /// on their own rather than being rewritten.
    ///
    /// This is emphatically **not** "the cluster is now healthy": it is only
    /// "the environment stops making it worse". Recovering from the damage is
    /// the simulated system's job.
    ///
    /// # It stays entered
    ///
    /// The promise holds for the rest of the run, not just for the instant it
    /// is made. Every setter that installs a caller-supplied fault
    /// configuration — [`set_network_config`](Self::set_network_config),
    /// [`set_storage_config`](Self::set_storage_config),
    /// [`set_process_storage_config`](Self::set_process_storage_config) — strips the
    /// fault knobs out of what it is handed once recovery mode is on, while
    /// installing the performance half unchanged. Reconfiguring a disk or link
    /// in the quiet tail therefore cannot re-arm chaos.
    ///
    /// The directed fault APIs stay callable after the cutoff: a caller that
    /// reaches for [`partition_pair`](Self::partition_pair),
    /// [`simulate_crash_for_process`](Self::simulate_crash_for_process), or the
    /// targeted storage-fault methods
    /// ([`corrupt_file`](Self::corrupt_file), [`fail_file_with_eio`](Self::fail_file_with_eio))
    /// is scripting a specific fault by hand rather than asking the simulator to
    /// generate one, and red tests depend on that still working. Note that a
    /// directed partition already in force is healed like any other at the
    /// cutoff (see *What is healed*); only the API remains available.
    ///
    /// Idempotent, and consumes no randomness.
    ///
    /// [`chaos_duration`]: crate::SimulationBuilder::chaos_duration
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[instrument(level = "debug", skip(self))]
    pub fn enter_recovery_mode(&mut self) {
        let mut inner = self.inner.write();
        if inner.recovery_mode {
            return;
        }
        inner.recovery_mode = true;
        inner.buggified_delay_window = BuggifiedDelayWindow::Inactive;
        // A gray failure is chaos: the quiet tail runs on healthy machines.
        // So is a full disk: the storage cutoff below lifts every capacity.
        inner.clear_slowness();
        inner.network.disable_fault_injection();
        inner.storage.disable_fault_injection();
        let now = inner.now();
        let actions = inner.network.heal_all_partitions(now);
        inner.apply_network(actions);
    }

    /// Whether [`enter_recovery_mode`](Self::enter_recovery_mode) has run.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn is_in_recovery_mode(&self) -> bool {
        self.inner.read().recovery_mode
    }

    pub(crate) fn poll_sleep(&self, task_id: u64, waker: &Waker) -> bool {
        let mut inner = self.inner.write();
        if inner.awakened_tasks.remove(&task_id) {
            inner.wakers.tasks.take(&task_id);
            true
        } else {
            inner.wakers.tasks.register(task_id, waker);
            false
        }
    }

    pub(crate) fn cancel_sleep(&self, task_id: u64, schedule_id: ScheduleId) {
        let mut inner = self.inner.write();
        inner.scheduler.cancel(schedule_id);
        inner.timer_schedules.remove(&task_id);
        inner.wakers.tasks.take(&task_id);
        inner.awakened_tasks.remove(&task_id);
    }

    /// Schedules a process restart.
    #[instrument(level = "debug", skip(self))]
    pub fn schedule_process_restart(&self, ip: IpAddr, recovery_delay: Duration) {
        self.schedule_event(Event::ProcessRestart { ip }, recovery_delay);
    }
}

impl Default for SimWorld {
    fn default() -> Self {
        Self::new()
    }
}

/// Non-owning handle to a simulation world.
#[derive(Debug, Clone)]
pub struct WeakSimWorld {
    pub(crate) inner: Weak<WorldLock<SimInner>>,
}

impl WeakSimWorld {
    /// Upgrades this handle while the simulation remains alive.
    pub(crate) fn upgrade(&self) -> SimulationResult<SimWorld> {
        self.inner
            .upgrade()
            .map(|inner| SimWorld { inner })
            .ok_or(SimulationError::SimulationShutdown)
    }

    /// Returns exact current logical time.
    pub(crate) fn now(&self) -> SimulationResult<Duration> {
        Ok(self.upgrade()?.now())
    }
    /// Returns a drifted timer reading.
    pub(crate) fn timer(&self) -> SimulationResult<Duration> {
        Ok(self.upgrade()?.timer())
    }
    /// Creates a sleep future.
    pub(crate) fn sleep(&self, duration: Duration) -> SimulationResult<SleepFuture> {
        Ok(self.upgrade()?.sleep(duration))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        net::IpAddr,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Poll, Wake, Waker},
        time::Duration,
    };

    use super::{Event, SimWorld};
    use crate::{
        LatencyDistribution, NetworkConfiguration, NetworkProvider, PartitionStrategy,
        SimulationError, TcpListenerTrait,
    };

    struct WakeCounter(AtomicUsize);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    use futures::{
        future::poll_fn,
        io::{AsyncReadExt, AsyncWriteExt},
    };

    async fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
        futures::pin_mut!(future);
        poll_fn(|context| match future.as_mut().poll(context) {
            Poll::Ready(output) => Poll::Ready(output),
            Poll::Pending if sim.has_pending_events() => {
                sim.step();
                context.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Pending => Poll::Pending,
        })
        .await
    }

    #[test]
    fn run_until_empty_completes_more_than_fifty_network_operations() {
        let mut sim = SimWorld::new();
        let mut delays = (0..51)
            .map(|_| {
                Box::pin(
                    sim.network_delay(Duration::from_millis(1))
                        .expect("network operation should schedule"),
                )
            })
            .collect::<Vec<_>>();
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        for delay in &mut delays {
            assert!(delay.as_mut().poll(&mut context).is_pending());
        }

        sim.run_until_empty();

        assert!(!sim.has_pending_events());
        for delay in &mut delays {
            assert!(matches!(
                delay.as_mut().poll(&mut context),
                Poll::Ready(Ok(()))
            ));
        }
    }

    #[test]
    fn buggified_delay_only_draws_inside_campaign_window() {
        let mut config = NetworkConfiguration::fast_local();
        config.chaos.buggified_delay_enabled = true;
        config.chaos.buggified_delay_probability = 1.0;
        config.chaos.buggified_delay_max = Duration::from_secs(1);
        let mut sim = SimWorld::new_with_network_config(config);

        sim.prepare_buggified_delay_campaign();
        let setup_sleep = sim.sleep(Duration::from_millis(1));
        assert_eq!(crate::sim::rng_call_count(), 0);
        drop(setup_sleep);

        sim.start_buggified_delay_window(Some(Duration::from_millis(5)));
        let chaos_sleep = sim.sleep(Duration::from_millis(1));
        assert_eq!(crate::sim::rng_call_count(), 2);
        drop(chaos_sleep);

        sim.schedule_event(Event::Timer { task_id: u64::MAX }, Duration::from_millis(5));
        assert!(!sim.step());
        let draws_at_cutoff = crate::sim::rng_call_count();
        let quiet_start = sim.now();
        let quiet_sleep = sim.sleep(Duration::from_millis(1));
        assert_eq!(crate::sim::rng_call_count(), draws_at_cutoff);
        sim.run_until_empty();
        assert_eq!(
            sim.now().saturating_sub(quiet_start),
            Duration::from_millis(1)
        );
        drop(quiet_sleep);
    }

    #[test]
    fn disabled_buggified_delay_preserves_rng_position_with_campaign_window() {
        let mut config = NetworkConfiguration::fast_local();
        config.chaos.buggified_delay_enabled = false;
        let sim = SimWorld::new_with_network_config(config);

        sim.prepare_buggified_delay_campaign();
        sim.start_buggified_delay_window(Some(Duration::from_secs(1)));
        let sleep = sim.sleep(Duration::from_millis(1));

        assert_eq!(crate::sim::rng_call_count(), 0);
        drop(sleep);
    }

    #[test]
    fn shutdown_cancels_real_waiters_without_synthetic_timer_ids() {
        let mut sim = SimWorld::new();
        let mut sleep = Box::pin(sim.sleep(Duration::from_mins(1)));
        let mut network = Box::pin(
            sim.network_delay(Duration::from_mins(1))
                .expect("network operation should schedule"),
        );
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(sleep.as_mut().poll(&mut context).is_pending());
        assert!(network.as_mut().poll(&mut context).is_pending());
        let (client, server) = sim.create_connection_pair("127.0.0.1:1", "127.0.0.1:2");
        let waiter = std::task::Waker::noop().clone();
        let accept_waiter = sim
            .allocate_accept_waiter()
            .expect("accept waiter should allocate");
        assert!(matches!(
            sim.poll_accept("shutdown-listener", accept_waiter, waiter.clone()),
            Ok(None)
        ));
        assert!(sim.register_read_waker(server, &waiter));
        assert!(sim.register_clog_waker(client, &waiter));
        assert!(sim.register_read_clog_waker(server, &waiter));
        assert!(sim.register_send_buffer_waker(client, &waiter));
        sim.schedule_event(
            Event::Network(crate::network::sim::NetworkEvent::Maintenance),
            Duration::from_mins(2),
        );

        sim.schedule_event(Event::Shutdown, Duration::from_nanos(1));
        sim.step();

        {
            let inner = sim.inner.read();
            assert_eq!(inner.awakened_tasks, [0].into());
            assert!(inner.timer_schedules.is_empty());
            assert!(inner.network_schedules.is_empty());
            assert!(inner.scheduler.is_empty());
        }
        assert!(matches!(
            sleep.as_mut().poll(&mut context),
            Poll::Ready(Ok(()))
        ));
        assert!(matches!(
            network.as_mut().poll(&mut context),
            Poll::Ready(Err(SimulationError::SimulationShutdown))
        ));
        let waiter = std::task::Waker::noop().clone();
        assert!(matches!(
            sim.poll_accept("shutdown-listener", accept_waiter, waiter.clone()),
            Err(SimulationError::SimulationShutdown)
        ));
        assert!(!sim.register_read_waker(server, &waiter));
        assert!(!sim.register_clog_waker(client, &waiter));
        assert!(!sim.register_read_clog_waker(server, &waiter));
        assert!(!sim.register_send_buffer_waker(client, &waiter));
        assert!(sim.inner.read().awakened_tasks.is_empty());
    }

    #[test]
    fn shutdown_resolves_a_sleep_that_was_never_polled() {
        let mut sim = SimWorld::new();
        let mut polled = Box::pin(sim.sleep(Duration::from_mins(1)));
        let mut stashed = Box::pin(sim.sleep(Duration::from_mins(1)));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(polled.as_mut().poll(&mut context).is_pending());

        sim.schedule_event(Event::Shutdown, Duration::ZERO);
        assert!(!sim.step());
        assert!(sim.inner.read().timer_schedules.is_empty());

        // The stashed sleep's timer was cancelled before it ever registered a
        // waker; its first poll must still complete rather than pend forever.
        assert!(matches!(
            stashed.as_mut().poll(&mut context),
            Poll::Ready(Ok(()))
        ));
        assert!(matches!(
            polled.as_mut().poll(&mut context),
            Poll::Ready(Ok(()))
        ));
        assert!(sim.inner.read().awakened_tasks.is_empty());
    }

    #[test]
    fn shutdown_fails_an_operation_that_fired_before_its_future_repolled() {
        let mut sim = SimWorld::new();
        let mut network = Box::pin(
            sim.network_delay(Duration::from_nanos(1))
                .expect("network operation should schedule"),
        );
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(network.as_mut().poll(&mut context).is_pending());
        assert!(!sim.step());

        sim.schedule_event(Event::Shutdown, Duration::ZERO);
        assert!(!sim.step());

        assert!(matches!(
            network.as_mut().poll(&mut context),
            Poll::Ready(Err(SimulationError::SimulationShutdown))
        ));
    }

    #[test]
    fn shutdown_fails_an_accept_reserved_but_not_repolled() {
        let mut sim = SimWorld::new();
        let waiter_id = sim
            .allocate_accept_waiter()
            .expect("accept waiter should allocate");
        let waiter = std::task::Waker::noop().clone();
        assert!(matches!(
            sim.poll_accept("reserved", waiter_id, waiter.clone()),
            Ok(None)
        ));
        let owner: std::net::IpAddr = "127.0.0.1".parse().expect("valid IP");
        let _listener = sim
            .bind_listener("reserved", owner)
            .expect("the address is free");
        let (_, server) = sim.create_connection_pair("127.0.0.1:1", "127.0.0.1:2");
        assert!(sim.store_pending_connection("reserved", server));

        sim.schedule_event(Event::Shutdown, Duration::ZERO);
        assert!(!sim.step());

        assert!(matches!(
            sim.poll_accept("reserved", waiter_id, waiter),
            Err(SimulationError::SimulationShutdown)
        ));
    }

    #[test]
    fn cancelling_the_first_accept_transfers_its_reservation_fifo() {
        let sim = SimWorld::new();
        let first = sim
            .allocate_accept_waiter()
            .expect("first accept waiter should allocate");
        let second = sim
            .allocate_accept_waiter()
            .expect("second accept waiter should allocate");
        let waiter = std::task::Waker::noop().clone();
        assert!(matches!(
            sim.poll_accept("fifo", first, waiter.clone()),
            Ok(None)
        ));
        assert!(matches!(
            sim.poll_accept("fifo", second, waiter.clone()),
            Ok(None)
        ));
        let owner: std::net::IpAddr = "127.0.0.1".parse().expect("valid IP");
        let _listener = sim
            .bind_listener("fifo", owner)
            .expect("the address is free");
        let (_, server) = sim.create_connection_pair("127.0.0.1:1", "127.0.0.1:2");
        assert!(sim.store_pending_connection("fifo", server));

        sim.cancel_accept("fifo", first);

        assert!(matches!(
            sim.poll_accept("fifo", second, waiter),
            Ok(Some(connection)) if connection == server
        ));
    }

    #[test]
    fn direct_pending_publication_obeys_backlog_capacity() {
        let mut config = NetworkConfiguration::fast_local();
        config.accept_backlog_capacity = 1;
        let sim = SimWorld::new_with_network_config(config);
        let owner: std::net::IpAddr = "127.0.0.1".parse().expect("valid IP");
        let _listener = sim
            .bind_listener("bounded", owner)
            .expect("bind logical address");
        let (_, first_server) = sim.create_connection_pair("127.0.0.1:1", "127.0.0.1:2");
        let (_, second_server) = sim.create_connection_pair("127.0.0.1:3", "127.0.0.1:4");
        assert!(sim.store_pending_connection("bounded", first_server));
        assert!(
            !sim.store_pending_connection("bounded", second_server),
            "direct callers cannot overfill the listener"
        );
        sim.discard_connection_pair(second_server);
    }

    #[test]
    fn cancelling_connect_discards_its_preallocated_pair() {
        let mut config = NetworkConfiguration::fast_local();
        config.connect_latency = LatencyDistribution::Uniform {
            start: Duration::from_mins(1),
            end: Duration::from_mins(1),
        };
        let sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider(IpAddr::from([127, 0, 0, 1]));
        let mut connect = Box::pin(provider.connect("cancel-connect"));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(connect.as_mut().poll(&mut context).is_pending());
        assert_eq!(sim.inner.read().network.connection_count(), 2);

        drop(connect);

        assert_eq!(sim.inner.read().network.connection_count(), 0);
        assert!(!sim.has_pending_events());
    }

    #[test]
    fn abort_discards_a_queued_send_and_deduplicates_its_waiter() {
        let mut sim = SimWorld::new();
        let (client, server) = sim.create_connection_pair("127.0.0.1:1", "127.0.0.1:2");
        sim.buffer_send(client, b"must not arrive".to_vec())
            .expect("send should queue");
        let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let waiter = Waker::from(Arc::clone(&counter));
        for _ in 0..100 {
            assert!(sim.register_send_buffer_waker(client, &waiter));
        }

        sim.close_connection_abort(client);
        sim.run_until_empty();

        let mut buffer = [0; 32];
        assert_eq!(
            sim.read_from_connection(server, &mut buffer)
                .expect("read should inspect the peer buffer"),
            0
        );
        assert!(sim.take_faults().is_empty());
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
        assert!(!sim.register_send_buffer_waker(client, &waiter));
    }

    #[test]
    fn closed_connections_are_not_partition_candidates() {
        let mut config = NetworkConfiguration::fast_local();
        config.chaos.partition_rate = f64::INFINITY;
        config.chaos.partition_strategy = PartitionStrategy::IsolateSingle;
        let mut sim = SimWorld::new_with_network_config(config);
        let first = IpAddr::from([10, 0, 0, 1]);
        let second = IpAddr::from([10, 0, 0, 2]);
        let (client, _) = sim.create_connection_pair(&format!("{first}:1"), &format!("{second}:2"));
        sim.close_connection_abort(client);
        sim.schedule_event(
            Event::Network(crate::network::sim::NetworkEvent::Maintenance),
            Duration::ZERO,
        );

        assert!(!sim.step());

        assert!(!sim.is_partitioned(first, second));
        assert!(!sim.is_partitioned(second, first));
        assert!(sim.take_faults().is_empty());
    }

    #[test]
    fn network_can_be_reused_after_shutdown() {
        // Simulated providers run on the moonpool executor, never on tokio.
        crate::executor::Executor::new(0).block_on(async {
            let mut sim = SimWorld::new();
            let provider = sim.network_provider(IpAddr::from([127, 0, 0, 1]));
            let _old_listener = drive(&mut sim, provider.bind("after-shutdown"))
                .await
                .expect("initial bind");
            let _unaccepted = drive(&mut sim, provider.connect("after-shutdown"))
                .await
                .expect("initial connect");
            sim.schedule_event(Event::Shutdown, Duration::ZERO);
            assert!(!sim.step());

            let listener = drive(&mut sim, provider.bind("after-shutdown"))
                .await
                .expect("bind after shutdown");
            let mut client = drive(&mut sim, provider.connect("after-shutdown"))
                .await
                .expect("connect after shutdown");
            let (mut server, _) = drive(&mut sim, listener.accept())
                .await
                .expect("accept after shutdown");
            client.write_all(b"ok").await.expect("write after shutdown");
            let mut received = [0; 2];
            drive(&mut sim, server.read_exact(&mut received))
                .await
                .expect("read after shutdown");
            assert_eq!(&received, b"ok");
        });
    }
}
