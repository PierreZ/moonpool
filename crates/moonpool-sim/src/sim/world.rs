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
    events::{Event, ScheduleId, Scheduler},
    rng::{reset_sim_rng, set_sim_seed, sim_random},
    sleep::SleepFuture,
    wakers::Wakers,
};
use crate::storage::sim::{OperationId, StorageEngine};

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

/// State shared by the scheduler and the independent resource engines.
#[derive(Debug)]
pub(crate) struct SimInner {
    pub(crate) scheduler: Scheduler<Event>,
    pub(crate) timer_time: Duration,
    pub(crate) network: NetworkSimulation,
    pub(crate) network_schedules: BTreeMap<NetworkOperationId, ScheduleId>,
    pub(crate) storage: StorageEngine,
    pub(crate) storage_schedules: BTreeMap<OperationId, ScheduleId>,
    pub(crate) block: crate::storage::block::registry::BlockDeviceRegistry,
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
            block: crate::storage::block::registry::BlockDeviceRegistry::default(),
            wakers: Wakers::default(),
            timer_schedules: BTreeMap::new(),
            next_task_id: 0,
            awakened_tasks: BTreeSet::new(),
            events_processed: 0,
            last_processed_event: None,
            pending_faults: Vec::new(),
            buggified_delay_window: BuggifiedDelayWindow::Unbounded,
            recovery_mode: false,
        }
    }

    pub(crate) fn now(&self) -> Duration {
        self.scheduler.now()
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
        if let Err(error) = self.scheduler.schedule_after(delay, event) {
            tracing::error!(%error, "failed to schedule simulation event");
        }
    }

    pub(crate) fn schedule_at(&mut self, event: Event, time: Duration) {
        if let Err(error) = self.scheduler.schedule_at(time, event) {
            tracing::error!(%error, "failed to schedule simulation event");
        }
    }

    pub(crate) fn record_fault(&mut self, event: SimFaultEvent) {
        let time_ms = u64::try_from(self.now().as_millis()).unwrap_or(u64::MAX);
        self.pending_faults.push(SimFaultRecord { time_ms, event });
    }

    pub(crate) fn apply_network(&mut self, actions: NetworkActions) {
        let (scheduled, faults) = actions.into_parts();
        for (at, event) in scheduled {
            self.schedule_at(Event::Network(event), at);
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

/// Global deterministic simulation coordinator.
#[derive(Debug)]
pub struct SimWorld {
    pub(crate) inner: Arc<WorldLock<SimInner>>,
}

impl SimWorld {
    fn create(network_config: NetworkConfiguration, seed: u64) -> Self {
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
    pub fn new_with_network_config_and_seed(config: NetworkConfiguration, seed: u64) -> Self {
        Self::create(config, seed)
    }

    /// Processes one delayed event and returns whether another event is queued.
    #[instrument(skip(self))]
    pub fn step(&mut self) -> bool {
        let (processed, wakes) = {
            let mut inner = self.inner.write();
            let Some(scheduled) = inner.scheduler.pop() else {
                inner.last_processed_event = None;
                return false;
            };
            let now = inner.now();
            let (actions, mut wakes) = inner.network.before_event(now);
            inner.apply_network(actions);

            let event = scheduled.into_value();
            inner.last_processed_event = Some(event.clone());
            inner.events_processed += 1;
            match event {
                Event::Timer { task_id } => {
                    inner.timer_schedules.remove(&task_id);
                    inner.awakened_tasks.insert(task_id);
                    wakes.push(inner.wakers.tasks.remove(&task_id));
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
                    let timer_schedules = std::mem::take(&mut inner.timer_schedules);
                    for schedule_id in timer_schedules.into_values() {
                        inner.scheduler.cancel(schedule_id);
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
                    let storage_actions = inner.storage.shutdown();
                    wakes.append(super::storage_ops::apply_storage_actions(
                        &mut inner,
                        storage_actions,
                    ));
                }
                Event::ProcessRestart { .. }
                | Event::ProcessGracefulShutdown { .. }
                | Event::ProcessForceKill { .. } => {}
            }
            (true, wakes)
        };
        wakes.wake();
        processed && !self.inner.read().scheduler.is_empty()
    }

    /// Processes queued workload events until stalled.
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
    pub fn timer(&self) -> Duration {
        let mut inner = self.inner.write();
        let chaos = &inner.network.config().chaos;
        if !chaos.clock_drift_enabled {
            // Never hand back a reading below one already given out. Drift that
            // accumulated before it was switched off (recovery mode) is a real
            // skew the node still has to live with; rewinding the clock instead
            // would be a *new* fault, and a nastier one than the drift.
            inner.timer_time = inner.timer_time.max(inner.now());
            return inner.timer_time;
        }
        let max_timer = inner.now().saturating_add(chaos.clock_drift_max);
        if inner.timer_time < max_timer {
            let gap = max_timer.saturating_sub(inner.timer_time).as_secs_f64();
            inner.timer_time += Duration::from_secs_f64(sim_random::<f64>() * gap / 2.0);
        }
        inner.timer_time = inner.timer_time.max(inner.now());
        inner.timer_time
    }

    /// Schedules an event after a relative delay.
    pub fn schedule_event(&self, event: Event, delay: Duration) {
        self.inner.write().schedule_after(event, delay);
    }

    /// Schedules an event at an absolute logical time.
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

    /// Creates a task provider.
    #[must_use]
    pub fn task_provider(&self) -> crate::providers::SimTaskProvider {
        crate::providers::SimTaskProvider
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
    pub fn set_storage_config(&mut self, mut config: crate::storage::StorageConfiguration) {
        let mut inner = self.inner.write();
        if inner.recovery_mode() {
            config.disable_fault_injection();
        }
        inner.storage.set_config(config);
    }

    /// Replaces storage configuration for one IP.
    ///
    /// Recovery-aware in the same way as
    /// [`set_storage_config`](Self::set_storage_config).
    pub fn set_storage_config_for(
        &mut self,
        ip: IpAddr,
        mut config: crate::storage::StorageConfiguration,
    ) {
        let mut inner = self.inner.write();
        if inner.recovery_mode() {
            config.disable_fault_injection();
        }
        inner.storage.set_config_for(ip, config);
    }

    /// Returns an active disk episode for one IP.
    #[must_use]
    pub fn disk_episode_for(
        &self,
        ip: IpAddr,
    ) -> Option<crate::storage::sim::DiskDegradationState> {
        self.inner.read().storage.disk_episode_for(ip)
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
        match inner
            .scheduler
            .schedule_after(duration, Event::Timer { task_id })
        {
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
    /// - storage: read/write/sync/crash faults, misdirected and phantom
    ///   writes, and new disk stall or throttle episodes;
    /// - block devices: EIO, read corruption, misdirected and phantom writes,
    ///   persist failures, and barrier violations.
    ///
    /// # What is healed
    ///
    /// Every partition currently in force — directed pair cuts and the
    /// asymmetric send-side and receive-side blocks alike. A partition is
    /// environmental: nothing in the system under test can clear it, so
    /// leaving one up would make the quiet tail untestable rather than quiet.
    ///
    /// # What survives
    ///
    /// Everything already done. Corrupted sectors stay corrupted, lost writes
    /// stay lost, misdirected and phantom writes are not undone, a connection
    /// the application already saw close stays closed, a process already
    /// killed is not resurrected, and application state is left exactly as the
    /// chaos phase left it. Finite effects already started — a disk stall or
    /// throttle episode, a clog, a packet already scheduled with delay — keep
    /// their deadlines and expire on their own rather than being rewritten.
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
    /// [`set_storage_config_for`](Self::set_storage_config_for),
    /// [`set_process_storage_config`](Self::set_process_storage_config),
    /// [`set_block_fault_config`](Self::set_block_fault_config) — strips the
    /// fault knobs out of what it is handed once recovery mode is on, while
    /// installing the performance half unchanged. Reconfiguring a disk or link
    /// in the quiet tail therefore cannot re-arm chaos.
    ///
    /// Directed fault APIs are deliberately left alone: a caller that reaches
    /// for [`partition_pair`](Self::partition_pair),
    /// [`simulate_crash_for_process`](Self::simulate_crash_for_process), or the
    /// [`SimBlockStore`](crate::SimBlockStore) targeted-fault methods is
    /// scripting a specific fault by hand rather than asking the simulator to
    /// generate one, and red tests depend on that still working.
    ///
    /// Idempotent, and consumes no randomness.
    ///
    /// [`chaos_duration`]: crate::SimulationBuilder::chaos_duration
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn enter_recovery_mode(&mut self) {
        let mut inner = self.inner.write();
        if inner.recovery_mode {
            return;
        }
        inner.recovery_mode = true;
        inner.buggified_delay_window = BuggifiedDelayWindow::Inactive;
        inner.network.disable_fault_injection();
        inner.storage.disable_fault_injection();
        inner.block.disable_fault_injection();
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
            inner.wakers.tasks.remove(&task_id);
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
        inner.wakers.tasks.remove(&task_id);
        inner.awakened_tasks.remove(&task_id);
    }

    /// Schedules a process restart.
    pub fn schedule_process_restart(&self, ip: IpAddr, recovery_delay: Duration) {
        self.schedule_event(Event::ProcessRestart { ip }, recovery_delay);
    }

    /// Returns all tracked assertion results.
    #[must_use]
    pub fn assertion_results(
        &self,
    ) -> std::collections::BTreeMap<String, crate::chaos::AssertionStats> {
        crate::chaos::assertion_results()
    }

    /// Clears assertion statistics.
    pub fn reset_assertion_results(&self) {
        crate::chaos::reset_assertion_results();
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
        assert!(sim.register_read_waker(server, waiter.clone()));
        assert!(sim.register_clog_waker(client, waiter.clone()));
        assert!(sim.register_read_clog_waker(server, waiter.clone()));
        assert!(sim.register_send_buffer_waker(client, waiter));
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
        assert!(!sim.register_read_waker(server, waiter.clone()));
        assert!(!sim.register_clog_waker(client, waiter.clone()));
        assert!(!sim.register_read_clog_waker(server, waiter.clone()));
        assert!(!sim.register_send_buffer_waker(client, waiter));
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
        let (_, server) = sim.create_connection_pair("127.0.0.1:1", "127.0.0.1:2");
        sim.store_pending_connection("reserved", server);

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
        let (_, server) = sim.create_connection_pair("127.0.0.1:1", "127.0.0.1:2");
        sim.store_pending_connection("fifo", server);

        sim.cancel_accept("fifo", first);

        assert!(matches!(
            sim.poll_accept("fifo", second, waiter),
            Ok(Some(connection)) if connection == server
        ));
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
            assert!(sim.register_send_buffer_waker(client, waiter.clone()));
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
        assert!(!sim.register_send_buffer_waker(client, waiter));
    }

    #[test]
    fn closed_connections_are_not_partition_candidates() {
        let mut config = NetworkConfiguration::fast_local();
        config.chaos.partition_probability = 1.0;
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
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
