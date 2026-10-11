//! The simulated CPU: one core per process, and the gate the executor asks
//! before it polls a process's task.
//!
//! Without a CPU model a task poll takes no simulated time: time moves only
//! in [`SimWorld::step`]. With one ([`SimWorld::set_cpu_model`]), every
//! simulated process (a task owner, `owner: Some(ip)`) has one CPU slot with
//! a `free_at` instant, and every poll of one of its tasks costs CPU time:
//!
//! - A poll may start only when the slot is free at the scheduler's clock.
//!   Otherwise the executor hands the runnable to the world, which parks it
//!   per process and schedules [`Event::CpuFree`] at `free_at`; that event
//!   puts the parked runnables back on the ready queue. A parked task is not
//!   runnable, so `until_stalled` keeps its contract, and the pending
//!   `CpuFree` is a scheduled event, which the stall detector counts as
//!   progress.
//! - At poll start the process's **local clock** is `now + poll_cost ×
//!   slowness`. Every byte it sends or writes, and every explicit
//!   [`cpu_charge`](moonpool_buggify::cpu_charge), moves the local clock on.
//!   At poll end the slot is free at the local clock.
//! - While a poll is active, `SimInner::now` is the local clock. So the
//!   timers, storage submits and the time provider of the process see the
//!   CPU time; what the process does happens after the work.
//! - The network engine keeps the scheduler's clock (a connection's clog and
//!   bandwidth windows need one clock on both sides). The world shifts every
//!   network event a poll schedules by `local − now`, and a sent chunk goes
//!   on the wire no earlier than its writer's local clock.
//! - A wake that a poll causes directly (a read that frees the peer's send
//!   window, a cancel, a close) is deferred to the local clock through
//!   [`Event::CpuWake`].
//!
//! A task without an owner (the driver, a workload, a fault injector) is
//! never gated and costs nothing. Network and storage completions are world
//! events, not tasks, so they cost nothing either. Tasks a process spawns
//! inherit its owner: moonpool's own RPC tasks pay, because they are shipped
//! code that uses the machine's CPU in production too.

use std::{cell::RefCell, collections::BTreeMap, net::IpAddr, time::Duration};

use super::{
    events::{Event, ScheduleId},
    rng::{sim_random_bool, sim_random_range},
    wakers::WakeBatch,
    world::{SimInner, SimWorld, WeakSimWorld},
};
use crate::executor::ExecRunnable;

/// The per-seed costs of the simulated CPU (see [`SimWorld::set_cpu_model`]).
///
/// Every cost is multiplied by the process's CPU [`Slowness`] (1 when
/// healthy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuModel {
    /// The cost of one poll of one task of a process.
    pub poll_cost: Duration,
    /// The cost of one byte a process sends, in picoseconds.
    pub sent_byte_cost_ps: u64,
    /// The cost of one byte a process writes to its disk, in picoseconds.
    pub written_byte_cost_ps: u64,
}

impl CpuModel {
    /// The range [`random_for_seed`](Self::random_for_seed) draws the poll
    /// cost from, in nanoseconds (0.5 to 5 µs).
    pub const POLL_COST_NS: std::ops::Range<u64> = 500..5_001;

    /// The range [`random_for_seed`](Self::random_for_seed) draws each byte
    /// cost from, in picoseconds (0.5 to 5 ns per byte).
    pub const BYTE_COST_PS: std::ops::Range<u64> = 500..5_001;

    /// A model with the given costs.
    #[must_use]
    pub const fn new(
        poll_cost: Duration,
        sent_byte_cost_ps: u64,
        written_byte_cost_ps: u64,
    ) -> Self {
        Self {
            poll_cost,
            sent_byte_cost_ps,
            written_byte_cost_ps,
        }
    }

    /// The per-seed model: three draws from the simulation stream, the poll
    /// cost, then the sent and written byte costs.
    #[must_use]
    pub fn random_for_seed() -> Self {
        let poll = sim_random_range(Self::POLL_COST_NS);
        let sent = sim_random_range(Self::BYTE_COST_PS);
        let written = sim_random_range(Self::BYTE_COST_PS);
        Self::new(Duration::from_nanos(poll), sent, written)
    }

    /// The per-seed swarm draw: the model is on for half of the seeds (one
    /// draw), with [`random_for_seed`](Self::random_for_seed)'s costs (three
    /// more draws, only when on). Off is `None`: nothing changes.
    #[must_use]
    pub fn swarm_for_seed() -> Option<Self> {
        sim_random_bool(0.5).then(Self::random_for_seed)
    }

    /// The cost of `bytes` bytes at `per_byte_ps` picoseconds each.
    #[must_use]
    pub fn bytes_cost(per_byte_ps: u64, bytes: usize) -> Duration {
        let bytes = u128::try_from(bytes).unwrap_or(u128::MAX);
        let nanos = u128::from(per_byte_ps).saturating_mul(bytes) / 1_000;
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
}

/// How slow one machine is: the factors a gray failure sets
/// (see [`SimWorld::set_slowness`]).
///
/// Each factor multiplies one resource's time: 1 is healthy. The CPU factor
/// multiplies every CPU cost of the process (it has no effect while the CPU
/// model is off), the disk factor every latency of its disk, and the network
/// factor the latency of every chunk sent to or from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slowness {
    /// CPU time multiplier.
    pub cpu: u32,
    /// Disk latency multiplier.
    pub disk: u32,
    /// Network latency multiplier, to and from the machine.
    pub network: u32,
}

impl Slowness {
    /// A healthy machine: every factor is 1.
    pub const HEALTHY: Self = Self {
        cpu: 1,
        disk: 1,
        network: 1,
    };

    /// Whether every factor is 1.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        *self == Self::HEALTHY
    }
}

impl Default for Slowness {
    fn default() -> Self {
        Self::HEALTHY
    }
}

/// The poll that occupies a slot right now.
#[derive(Debug, Clone, Copy)]
struct ActivePoll {
    ip: IpAddr,
    /// The process's local clock: the poll's start plus every cost so far.
    local: Duration,
}

/// One process's CPU slot.
///
/// The busy time is `busy_done` (the polls that finished before the current
/// interval) plus the current interval `[start, free_at]`, read up to the
/// sample time only.
#[derive(Debug, Clone, Copy, Default)]
struct CpuSlot {
    start: Duration,
    free_at: Duration,
    busy_done: Duration,
    /// The pending [`Event::CpuFree`] of this slot, if any.
    free_event: Option<ScheduleId>,
}

impl CpuSlot {
    /// The CPU time used up to `now`: never time after `now`.
    fn busy(&self, now: Duration) -> Duration {
        self.busy_done + now.min(self.free_at).saturating_sub(self.start)
    }
}

/// The CPU model's state, owned by [`SimInner`].
#[derive(Default)]
pub(crate) struct CpuState {
    model: Option<CpuModel>,
    slots: BTreeMap<IpAddr, CpuSlot>,
    /// Runnables that met a busy slot, per process, in arrival order.
    parked: BTreeMap<IpAddr, Vec<ExecRunnable>>,
    active: Option<ActivePoll>,
    /// CPU slowness per process (absent is 1).
    slow: BTreeMap<IpAddr, u32>,
    /// Wakes a poll caused, held until its local clock.
    deferred: BTreeMap<u64, WakeBatch>,
    next_deferred: u64,
}

impl std::fmt::Debug for CpuState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpuState")
            .field("model", &self.model)
            .field("slots", &self.slots)
            .field(
                "parked",
                &self
                    .parked
                    .iter()
                    .map(|(ip, parked)| (*ip, parked.len()))
                    .collect::<Vec<_>>(),
            )
            .field("active", &self.active)
            .field("slow", &self.slow)
            .field("deferred", &self.deferred.len())
            .field("next_deferred", &self.next_deferred)
            .finish()
    }
}

impl CpuState {
    /// The local clock of the active poll, if one is active.
    pub(crate) fn local(&self) -> Option<Duration> {
        self.active.map(|active| active.local)
    }

    fn slow_factor(&self, ip: IpAddr) -> u32 {
        self.slow.get(&ip).copied().unwrap_or(1)
    }
}

impl SimInner {
    /// The scheduler's clock, whatever poll is active.
    pub(crate) fn scheduler_now(&self) -> Duration {
        self.scheduler.now()
    }

    /// How far the active poll's local clock is ahead of the scheduler's.
    pub(crate) fn cpu_offset(&self) -> Duration {
        self.cpu.local().map_or(Duration::ZERO, |local| {
            local.saturating_sub(self.scheduler.now())
        })
    }

    /// The active poll did `cost` of work: its local clock moves on by
    /// `cost × slowness`. Outside a process's poll, nothing happens.
    pub(crate) fn cpu_charge(&mut self, cost: Duration) {
        let Some(active) = self.cpu.active else {
            return;
        };
        let cost = cost.saturating_mul(self.cpu.slow_factor(active.ip));
        let local = active.local.saturating_add(cost);
        self.cpu.active = Some(ActivePoll { local, ..active });
        let slot = self.cpu.slots.entry(active.ip).or_default();
        assert!(
            slot.free_at == active.local,
            "cpu: the active poll owns its slot up to its local clock"
        );
        slot.free_at = local;
    }

    /// Charge the active poll for `bytes` bytes sent.
    pub(crate) fn cpu_charge_sent(&mut self, bytes: usize) {
        if let Some(model) = self.cpu.model {
            self.cpu_charge(CpuModel::bytes_cost(model.sent_byte_cost_ps, bytes));
        }
    }

    /// Charge the active poll for `bytes` bytes written.
    pub(crate) fn cpu_charge_written(&mut self, bytes: usize) {
        if let Some(model) = self.cpu.model {
            self.cpu_charge(CpuModel::bytes_cost(model.written_byte_cost_ps, bytes));
        }
    }

    /// The CPU time `ip` used up to `now`.
    pub(crate) fn cpu_busy(&self, ip: IpAddr, now: Duration) -> Duration {
        self.cpu
            .slots
            .get(&ip)
            .map_or(Duration::ZERO, |slot| slot.busy(now))
    }

    /// Defer `wakes` to the active poll's local clock. Gives them back when
    /// they may run now: no poll is active, or its clock is not ahead.
    pub(crate) fn cpu_defer_wakes(&mut self, wakes: WakeBatch) -> Option<WakeBatch> {
        let Some(local) = self.cpu.local() else {
            return Some(wakes);
        };
        if wakes.is_empty() || local <= self.scheduler.now() {
            return Some(wakes);
        }
        let id = self.cpu.next_deferred;
        self.cpu.next_deferred += 1;
        match self.schedule_effect_at(local, Event::CpuWake { id }) {
            Ok(_) => {
                self.cpu.deferred.insert(id, wakes);
                None
            }
            Err(error) => {
                tracing::error!(%error, "failed to defer a wake to the local clock");
                Some(wakes)
            }
        }
    }

    /// Make every machine healthy again (recovery mode).
    pub(crate) fn clear_slowness(&mut self) {
        self.cpu.slow.clear();
        self.storage.clear_slow_factors();
        self.network.clear_slow_factors();
    }

    /// The wakes deferred under `id`.
    pub(crate) fn cpu_take_deferred(&mut self, id: u64) -> WakeBatch {
        self.cpu.deferred.remove(&id).unwrap_or_default()
    }

    /// `ip`'s slot was due free: its parked runnables go back on the ready
    /// queue. A task woken at the same instant may have taken the slot
    /// first; the requeued runnables then meet the gate again and park
    /// behind it, with a new `CpuFree`.
    pub(crate) fn cpu_on_free(&mut self, ip: IpAddr) -> Vec<ExecRunnable> {
        if let Some(slot) = self.cpu.slots.get_mut(&ip) {
            slot.free_event = None;
        }
        self.cpu.parked.remove(&ip).unwrap_or_default()
    }

    /// Admit a poll of `owner`'s task, or park it until the slot is free.
    fn cpu_admit(&mut self, owner: IpAddr, runnable: ExecRunnable) -> Option<ExecRunnable> {
        let Some(model) = self.cpu.model else {
            return Some(runnable);
        };
        assert!(
            self.cpu.active.is_none(),
            "cpu: one poll at a time on the single-threaded executor"
        );
        let now = self.scheduler.now();
        let factor = self.cpu.slow_factor(owner);
        let slot = *self.cpu.slots.entry(owner).or_default();
        if slot.free_at > now {
            self.cpu.parked.entry(owner).or_default().push(runnable);
            if slot.free_event.is_none() {
                match self.schedule_effect_at(slot.free_at, Event::CpuFree { ip: owner }) {
                    Ok(id) => {
                        if let Some(slot) = self.cpu.slots.get_mut(&owner) {
                            slot.free_event = Some(id);
                        }
                    }
                    Err(error) => tracing::error!(%error, "failed to schedule CpuFree"),
                }
            }
            return None;
        }
        let local = now.saturating_add(model.poll_cost.saturating_mul(factor));
        let slot = self.cpu.slots.entry(owner).or_default();
        // The previous interval is over: its time is done.
        slot.busy_done += slot.free_at.saturating_sub(slot.start);
        slot.start = now;
        slot.free_at = local;
        self.cpu.active = Some(ActivePoll { ip: owner, local });
        Some(runnable)
    }

    /// The poll of `owner`'s task ended: its slot is free at its local clock.
    fn cpu_finish(&mut self, owner: IpAddr) {
        let Some(active) = self.cpu.active.take() else {
            return;
        };
        assert!(
            active.ip == owner,
            "cpu: the poll that ends is the active one"
        );
        let free_at = self.cpu.slots.get(&owner).map(|slot| slot.free_at);
        assert!(
            free_at == Some(active.local),
            "cpu: a slot is free at the local clock of its last poll"
        );
    }
}

impl SimWorld {
    /// Turn the CPU model on (`Some`) or off (`None`) for this world.
    ///
    /// With a model, every poll of a process's task takes CPU time on the
    /// process's one core (see the module docs of `moonpool_sim::sim::cpu`
    /// and the book's chapter on the CPU model). Off, which is the default,
    /// nothing changes: a poll takes no time. The runner sets it per seed
    /// from [`Chaos::Cpu`](crate::Chaos::Cpu); a raw `SimWorld` user sets it
    /// before any task runs.
    ///
    /// This world becomes the one the executor's gate and
    /// [`cpu_charge`](moonpool_buggify::cpu_charge) report to on this thread,
    /// until another world is created or the model is turned off.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn set_cpu_model(&self, model: Option<CpuModel>) {
        let parked = {
            let mut inner = self.inner.write();
            inner.cpu.model = model;
            if model.is_some() {
                Vec::new()
            } else {
                std::mem::take(&mut inner.cpu.parked)
                    .into_values()
                    .flatten()
                    .collect()
            }
        };
        if model.is_some() {
            install(self.downgrade());
        } else {
            uninstall();
        }
        for runnable in parked {
            runnable.schedule();
        }
    }

    /// The CPU model of this world, if it is on.
    #[must_use]
    pub fn cpu_model(&self) -> Option<CpuModel> {
        self.inner.read().cpu.model
    }

    /// The CPU time the process at `ip` used up to now (the scheduler's
    /// clock): the ground truth of the `process_cpu` its
    /// [`SimSystemProvider`](crate::SimSystemProvider) reports. Zero while
    /// the CPU model is off, and back to zero after a kill.
    #[must_use]
    pub fn cpu_busy(&self, ip: IpAddr) -> Duration {
        let inner = self.inner.read();
        inner.cpu_busy(ip, inner.scheduler_now())
    }

    /// Set how slow the machine at `ip` is (see [`Slowness`]).
    ///
    /// A gray failure: the machine stays alive and reachable, only slower.
    /// [`Chaos::GrayFailure`](crate::Chaos::GrayFailure) draws these per seed;
    /// a fault injector may set them by hand. The factors hold across a
    /// reboot of the process (they belong to the machine), and
    /// [`enter_recovery_mode`](Self::enter_recovery_mode) makes every machine
    /// healthy again.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn set_slowness(&self, ip: IpAddr, slowness: Slowness) {
        let mut inner = self.inner.write();
        if slowness.cpu <= 1 {
            inner.cpu.slow.remove(&ip);
        } else {
            inner.cpu.slow.insert(ip, slowness.cpu);
        }
        inner.storage.set_slow_factor(ip, slowness.disk);
        inner.network.set_slow_factor(ip, slowness.network);
    }

    /// How slow the machine at `ip` is now.
    #[must_use]
    pub fn slowness(&self, ip: IpAddr) -> Slowness {
        let inner = self.inner.read();
        Slowness {
            cpu: inner.cpu.slow_factor(ip),
            disk: inner.storage.slow_factor(ip),
            network: inner.network.slow_factor(ip),
        }
    }

    /// The process at `ip` was killed: drop its parked runnables (which
    /// cancels those tasks) and reset its slot, so a reboot starts with a
    /// free core and zero counters.
    pub(crate) fn reset_cpu_owner(&self, ip: IpAddr) {
        let parked = {
            let mut inner = self.inner.write();
            if let Some(slot) = inner.cpu.slots.remove(&ip)
                && let Some(event) = slot.free_event
            {
                inner.scheduler.cancel(event);
            }
            inner.cpu.parked.remove(&ip).unwrap_or_default()
        };
        // Dropped outside the lock: a task's destructor may call the world.
        drop(parked);
    }

    /// Release wakes a world call collected: at once, or at the local clock
    /// when a process's poll caused them.
    pub(crate) fn release_wakes(&self, wakes: WakeBatch) {
        if wakes.is_empty() {
            return;
        }
        let wakes = self.inner.write().cpu_defer_wakes(wakes);
        if let Some(wakes) = wakes {
            wakes.wake();
        }
    }
}

thread_local! {
    /// The world whose CPU model gates this thread's executor.
    static GATE: RefCell<Option<WeakSimWorld>> = const { RefCell::new(None) };
}

/// The sink [`moonpool_buggify::cpu_charge`] reports to while a model is on.
struct ChargeSink;

impl moonpool_buggify::cpu::Sink for ChargeSink {
    fn charge(&self, cost: Duration) {
        if let Some(world) = gate_world() {
            world.inner.write().cpu_charge(cost);
        }
    }
}

fn install(world: WeakSimWorld) {
    GATE.with(|gate| *gate.borrow_mut() = Some(world));
    moonpool_buggify::cpu::set_sink(Box::new(ChargeSink));
}

/// Remove this thread's gate: no poll is gated, no charge counts.
pub(crate) fn uninstall() {
    GATE.with(|gate| gate.borrow_mut().take());
    moonpool_buggify::cpu::clear_sink();
}

fn gate_world() -> Option<SimWorld> {
    GATE.with(|gate| gate.borrow().as_ref().and_then(|weak| weak.upgrade().ok()))
}

/// The executor asks before it polls a task of `owner`: the runnable back
/// to run it now, or `None` when the world parked it.
pub(crate) fn admit(owner: IpAddr, runnable: ExecRunnable) -> Option<ExecRunnable> {
    match gate_world() {
        Some(world) => world.inner.write().cpu_admit(owner, runnable),
        None => Some(runnable),
    }
}

/// The executor reports the end of a poll of a task of `owner`.
pub(crate) fn finish_poll(owner: IpAddr) {
    if let Some(world) = gate_world() {
        world.inner.write().cpu_finish(owner);
    }
}

/// Every parked runnable of this thread's gate, for the executor to drop
/// when it is dropped itself.
pub(crate) fn take_all_parked() -> Vec<ExecRunnable> {
    gate_world().map_or_else(Vec::new, |world| {
        std::mem::take(&mut world.inner.write().cpu.parked)
            .into_values()
            .flatten()
            .collect()
    })
}

#[cfg(test)]
mod tests;
