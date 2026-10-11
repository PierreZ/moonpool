//! Gray failures: a machine stays alive and reachable, and is slow for a
//! long window.
//!
//! A crash is easy to see: a peer times out and moves on. A slow machine is
//! the harder case: it answers, late. Heartbeats arrive, votes arrive, a
//! leader on it keeps its lease and drags the whole cluster down. The short
//! disk stall and throttle episodes of the storage chaos make spikes; a gray
//! failure pins one or more machines slow for a large part of the chaos
//! window.
//!
//! Three kinds, selected per seed by a swarm mask:
//!
//! - **CPU**: every CPU cost of the process is multiplied (only on seeds whose
//!   [CPU model](crate::CpuModel) is on; the gray failure never turns the
//!   model on);
//! - **disk**: every latency of the machine's disk is multiplied;
//! - **network**: the latency of every chunk sent to or from the machine is
//!   multiplied.
//!
//! The machine stays alive: no crash, no partition. At most
//! [`GrayFailure::max_slow`] machines are slow at once, so a quorum can still
//! form around them. [`SimWorld::enter_recovery_mode`](crate::SimWorld::enter_recovery_mode)
//! makes every machine healthy again, so the quiet tail is healthy.

use std::net::IpAddr;
use std::ops::{Range, RangeInclusive};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_core::TimeProvider;

use super::fault_injector::{FaultContext, FaultInjector};
use super::process::AttritionVictims;
use crate::sim::{Slowness, sim_random_bool, sim_random_f64, sim_random_range};
use crate::{SimulationResult, assert_reachable};

/// Which kinds of slowness a gray failure applies (the swarm mask).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrayKinds {
    /// Slow the machine's CPU (when the CPU model is on).
    pub cpu: bool,
    /// Slow the machine's disk.
    pub disk: bool,
    /// Slow the network to and from the machine.
    pub network: bool,
}

impl GrayKinds {
    /// Every kind.
    pub const ALL: Self = Self {
        cpu: true,
        disk: true,
        network: true,
    };
}

/// A gray-failure regime (see [`Chaos::GrayFailure`](crate::Chaos::GrayFailure)).
///
/// Each seed draws, when the chaos window opens, how many machines are slow
/// (one to `max_slow`, among the live and dead processes the `victims`
/// filter admits), which ones, and for each one a window and its factors.
/// The draws are a fixed footprint per victim, so a seed (or an exploration
/// recipe) replays the same schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct GrayFailure {
    /// Which processes may be slowed (as for attrition).
    pub victims: AttritionVictims,
    /// The budget: at most this many machines are slow at the same time.
    /// Keep it at the number of machines the system tolerates losing (`f`).
    pub max_slow: usize,
    /// The length of each machine's slow window, as a fraction of the chaos
    /// window (`0.3..=1.0` by default).
    pub window: RangeInclusive<f64>,
    /// The CPU factor range (`10..=1000` by default).
    pub cpu: RangeInclusive<u32>,
    /// The disk latency factor range (`5..=100` by default).
    pub disk: RangeInclusive<u32>,
    /// The network latency factor range (`2..=20` by default).
    pub network: RangeInclusive<u32>,
    /// Which kinds apply. [`ChaosMode::Swarm`](crate::ChaosMode::Swarm)
    /// masks each one per seed.
    pub kinds: GrayKinds,
}

impl GrayFailure {
    /// A regime that slows at most `max_slow` machines of any group, with
    /// the default windows, factors and every kind.
    #[must_use]
    pub fn new(max_slow: usize) -> Self {
        Self {
            victims: AttritionVictims::Any,
            max_slow,
            window: 0.3..=1.0,
            cpu: 10..=1000,
            disk: 5..=100,
            network: 2..=20,
            kinds: GrayKinds::ALL,
        }
    }

    /// The same regime, restricted to the processes `victims` admits.
    #[must_use]
    pub fn victims(self, victims: AttritionVictims) -> Self {
        Self { victims, ..self }
    }

    /// Why this regime cannot run, if it cannot.
    pub(crate) fn invalid(&self, registered: &[&str]) -> Option<String> {
        if let AttritionVictims::Group(group) = &self.victims
            && !registered.contains(&group.as_str())
        {
            return Some(format!(
                "a GrayFailure names the unregistered group '{group}'"
            ));
        }
        let (start, end) = (*self.window.start(), *self.window.end());
        if !(0.0..=1.0).contains(&start) || !(0.0..=1.0).contains(&end) || start > end {
            return Some(format!(
                "a GrayFailure window must lie in 0..=1 and not end before it starts (got {start}..={end})"
            ));
        }
        for (name, range) in [
            ("cpu", &self.cpu),
            ("disk", &self.disk),
            ("network", &self.network),
        ] {
            if *range.start() < 1 || range.start() > range.end() {
                return Some(format!(
                    "a GrayFailure {name} factor range must start at 1 or more and not end before it starts"
                ));
            }
        }
        None
    }

    /// The per-seed swarm regime: three draws, one mask bit per kind, always,
    /// so the draw count does not depend on the outcome. A seed whose mask
    /// keeps no kind makes no machine slow.
    pub(crate) fn swarm_for_seed(&self) -> Self {
        let cpu = sim_random_bool(0.5);
        let disk = sim_random_bool(0.5);
        let network = sim_random_bool(0.5);
        Self {
            kinds: GrayKinds {
                cpu: cpu && self.kinds.cpu,
                disk: disk && self.kinds.disk,
                network: network && self.kinds.network,
            },
            ..self.clone()
        }
    }
}

/// One machine's drawn gray window.
#[derive(Debug, Clone, Copy)]
struct SlowWindow {
    ip: IpAddr,
    start: Duration,
    end: Duration,
    slowness: Slowness,
}

/// The built-in injector behind [`Chaos::GrayFailure`](crate::Chaos::GrayFailure).
pub(crate) struct GrayFailureInjector {
    config: GrayFailure,
    chaos_duration: Duration,
}

impl GrayFailureInjector {
    pub(crate) fn new(config: GrayFailure, chaos_duration: Duration) -> Self {
        Self {
            config,
            chaos_duration,
        }
    }

    /// The processes the victim filter admits, in ascending IP order.
    fn pool(&self, ctx: &FaultContext) -> Vec<IpAddr> {
        let mut ips: Vec<IpAddr> = ctx
            .process_ips()
            .iter()
            .filter_map(|ip| ip.parse().ok())
            .filter(|ip| self.config.victims.admits(*ip, ctx.process_info()))
            .collect();
        ips.sort_unstable();
        ips.dedup();
        ips
    }

    /// Draw every slow window of this seed. The draws per victim are fixed:
    /// the window's length and start, then the three factors.
    fn draw_windows(&self, ctx: &FaultContext) -> Vec<SlowWindow> {
        let mut pool = self.pool(ctx);
        let budget = self.config.max_slow.min(pool.len());
        if budget == 0 {
            assert_reachable!("gray: no machine to slow");
            return Vec::new();
        }
        let count = sim_random_range(1..budget + 1);
        // A partial Fisher-Yates shuffle: the first `count` are the victims.
        for index in 0..count {
            let pick = sim_random_range(index..pool.len());
            pool.swap(index, pick);
        }
        let cpu_on = ctx.cpu_model().is_some();
        let kinds = self.config.kinds;
        pool.into_iter()
            .take(count)
            .map(|ip| {
                let (low, high) = (*self.config.window.start(), *self.config.window.end());
                let fraction = low + sim_random_f64() * (high - low);
                let length = self.chaos_duration.mul_f64(fraction);
                let slack = self.chaos_duration.saturating_sub(length);
                let start = slack.mul_f64(sim_random_f64());
                let cpu = draw_factor(&self.config.cpu);
                let disk = draw_factor(&self.config.disk);
                let network = draw_factor(&self.config.network);
                SlowWindow {
                    ip,
                    start,
                    end: start.saturating_add(length),
                    slowness: Slowness {
                        cpu: if kinds.cpu && cpu_on { cpu } else { 1 },
                        disk: if kinds.disk { disk } else { 1 },
                        network: if kinds.network { network } else { 1 },
                    },
                }
            })
            .collect()
    }
}

/// A uniform draw from `range`.
fn draw_factor(range: &RangeInclusive<u32>) -> u32 {
    sim_random_range(Range {
        start: *range.start(),
        end: range.end().saturating_add(1),
    })
}

/// Record the reach gates of one machine going slow.
fn reach_slow(slowness: Slowness) {
    if slowness.cpu > 1 {
        assert_reachable!("gray: cpu slow");
    }
    if slowness.disk > 1 {
        assert_reachable!("gray: disk slow");
    }
    if slowness.network > 1 {
        assert_reachable!("gray: network slow");
    }
}

#[async_trait]
impl FaultInjector for GrayFailureInjector {
    fn name(&self) -> &'static str {
        "gray_failure"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let windows = self.draw_windows(ctx);
        // The timeline: each window's start and end, in time order; a start
        // and an end at one instant apply in drawing order.
        let mut changes: Vec<(Duration, IpAddr, Slowness)> = windows
            .iter()
            .filter(|window| !window.slowness.is_healthy())
            .flat_map(|window| {
                [
                    (window.start, window.ip, window.slowness),
                    (window.end, window.ip, Slowness::HEALTHY),
                ]
            })
            .collect();
        if changes.is_empty() && !windows.is_empty() {
            assert_reachable!("gray: the seed's mask keeps no kind");
        }
        changes.sort_by_key(|(at, _, _)| *at);
        let origin = ctx.time().now();
        for (at, ip, slowness) in changes {
            let wait = origin.saturating_add(at).saturating_sub(ctx.time().now());
            ctx.time().sleep(wait).await.map_err(|error| {
                crate::SimulationError::InvalidState(format!("sleep failed: {error}"))
            })?;
            if ctx.chaos_shutdown().is_cancelled() {
                // Recovery mode made every machine healthy already.
                return Ok(());
            }
            ctx.set_slowness(&ip.to_string(), slowness)?;
            if slowness.is_healthy() {
                assert_reachable!("gray: machine healthy again");
                tracing::info!(%ip, "gray failure: machine healthy again");
            } else {
                reach_slow(slowness);
                tracing::info!(
                    %ip,
                    cpu = slowness.cpu,
                    disk = slowness.disk,
                    network = slowness.network,
                    "gray failure: machine slow"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{GrayFailure, GrayKinds};
    use crate::sim::rng::{rng_call_count, set_sim_seed};

    #[test]
    fn swarm_costs_three_draws_and_varies_the_mask() {
        set_sim_seed(9);
        let before = rng_call_count();
        let _ = GrayFailure::new(1).swarm_for_seed();
        assert_eq!(rng_call_count() - before, 3);

        let masks: Vec<GrayKinds> = (0..64_u64)
            .map(|seed| {
                set_sim_seed(seed);
                GrayFailure::new(1).swarm_for_seed().kinds
            })
            .collect();
        assert!(masks.contains(&GrayKinds::ALL));
        assert!(
            masks
                .iter()
                .any(|kinds| !kinds.cpu && !kinds.disk && !kinds.network)
        );
    }

    #[test]
    fn invalid_regimes_are_refused() {
        assert!(GrayFailure::new(1).invalid(&[]).is_none());
        let bad_window = GrayFailure {
            window: 0.5..=1.5,
            ..GrayFailure::new(1)
        };
        assert!(bad_window.invalid(&[]).is_some());
        let bad_factor = GrayFailure {
            disk: 0..=3,
            ..GrayFailure::new(1)
        };
        assert!(bad_factor.invalid(&[]).is_some());
        let unknown_group = GrayFailure::new(1).victims(crate::AttritionVictims::group("acceptor"));
        assert!(unknown_group.invalid(&["matchmaker"]).is_some());
        assert!(unknown_group.invalid(&["acceptor"]).is_none());
    }
}
