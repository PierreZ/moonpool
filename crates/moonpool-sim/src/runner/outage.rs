//! Correlated group outages: every process of one or more groups loses power
//! at the same instant, and each comes back after its own delay.
//!
//! [Attrition](super::process::Attrition) never takes more than `max_dead`
//! processes at once, by design. A rack-wide power loss is a different fault:
//! it empties every memory of a group at once, so what survives is exactly
//! what the disks hold, and no peer can repair a copy from memory first.
//! `FoundationDB`'s simulator models it as `killDataCenter`
//! (`sim2.actor.cpp`), which kills every machine of a datacenter in one loop
//! and lets each machine's own reboot delay bring it back.
//!
//! Each kill is a [`ProcessKillKind::Crash`](crate::ProcessKillKind::Crash)
//! that carries its own restart delay, so the restart is a scheduled
//! `ProcessRestart` event, not a step of the injector: it runs even once the
//! chaos window has closed and the injector is gone.

use std::net::IpAddr;
use std::ops::Range;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_core::TimeProvider;

use super::fault_injector::{FaultContext, FaultInjector};
use crate::{SimulationResult, assert_reachable};

/// The [`StateHandle`](crate::StateHandle) key an outage publishes its
/// [`OutageLanded`] under, once every victim is down.
pub const OUTAGE_STATE_KEY: &str = "moonpool.outage";

/// A correlated outage regime (see [`Chaos::Outage`](crate::Chaos::Outage)).
///
/// At most one outage per run, inside the chaos window: with probability
/// `probability`, at a time drawn from `start` (measured from the chaos
/// window's opening), every live process of `groups` crashes at the same
/// instant. Each one restarts after its own delay drawn from `down`, except
/// one **straggler** (when `straggler` is set), drawn from `straggler`.
///
/// Every runtime draw is made from the simulation stream, the probability
/// and start when the window opens, the delays at the outage instant in
/// ascending IP order, so a seed (or an exploration recipe) replays the same
/// kill and restart schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct Outage {
    /// The process groups (their [`Process::name`](crate::Process::name))
    /// that lose power together.
    pub groups: Vec<String>,
    /// The probability a seed has an outage at all.
    pub probability: f64,
    /// When the outage strikes, from the opening of the chaos window. A
    /// draw past the window's end means no outage.
    pub start: Range<Duration>,
    /// How long each victim stays down.
    pub down: Range<Duration>,
    /// How long the one straggler stays down, if there is one. Must start
    /// at or after `down`'s end, so the straggler is the last back.
    pub straggler: Option<Range<Duration>>,
}

impl Outage {
    /// An outage over `groups` that always strikes within the first ten
    /// seconds of the chaos window, keeps each victim down one to ten
    /// seconds, and has no straggler.
    #[must_use]
    pub fn groups<S: Into<String>>(groups: impl IntoIterator<Item = S>) -> Self {
        Self {
            groups: groups.into_iter().map(Into::into).collect(),
            probability: 1.0,
            start: Duration::ZERO..Duration::from_secs(10),
            down: Duration::from_secs(1)..Duration::from_secs(10),
            straggler: None,
        }
    }

    /// Why this regime cannot run, if it cannot.
    pub(crate) fn invalid(&self, registered: &[&str]) -> Option<String> {
        if self.groups.is_empty() {
            return Some("an Outage names no group".to_string());
        }
        if let Some(group) = self
            .groups
            .iter()
            .find(|group| !registered.contains(&group.as_str()))
        {
            return Some(format!("an Outage names the unregistered group '{group}'"));
        }
        if !(0.0..=1.0).contains(&self.probability) {
            return Some(format!(
                "an Outage probability must lie in 0..=1 (got {})",
                self.probability
            ));
        }
        if self.start.start > self.start.end || self.down.start > self.down.end {
            return Some("an Outage range ends before it starts".to_string());
        }
        if let Some(straggler) = &self.straggler
            && (straggler.start > straggler.end || straggler.start < self.down.end)
        {
            return Some(
                "an Outage straggler range must start at or after the end of `down`".to_string(),
            );
        }
        None
    }

    /// The per-seed swarm regime: two draws, always, so the draw count does
    /// not depend on the outcome. `None` is a seed the swarm leaves without
    /// an outage (and its run then makes no outage draw at all); a kept
    /// outage may lose its straggler.
    pub(crate) fn swarm_for_seed(&self) -> Option<Self> {
        let enabled = crate::sim::sim_random_bool(0.5);
        let keep_straggler = crate::sim::sim_random_bool(0.5);
        enabled.then(|| Self {
            straggler: self.straggler.clone().filter(|_| keep_straggler),
            ..self.clone()
        })
    }
}

/// What an outage did, published under [`OUTAGE_STATE_KEY`] in the
/// [`StateHandle`](crate::StateHandle) once every victim's kill has run, so
/// a fault injector (or a process, after its reboot) can act at the instant
/// every memory of the group is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutageLanded {
    /// The simulated time the kills were decided; each ran one tick later.
    pub at: Duration,
    /// The victims, in ascending IP order. A process already dead at the
    /// outage instant is not one: it comes back on its own schedule.
    pub victims: Vec<String>,
    /// Each victim's time down, parallel to `victims`.
    pub down: Vec<Duration>,
    /// The straggler, if this outage had one.
    pub straggler: Option<String>,
}

/// The built-in injector behind [`Chaos::Outage`](crate::Chaos::Outage).
pub(crate) struct OutageInjector {
    config: Outage,
}

impl OutageInjector {
    pub(crate) fn new(config: Outage) -> Self {
        Self { config }
    }

    /// The live processes of every configured group, in ascending IP order.
    fn victims(&self, ctx: &FaultContext) -> Vec<IpAddr> {
        let mut ips: Vec<IpAddr> = self
            .config
            .groups
            .iter()
            .flat_map(|group| ctx.group_registry().ips_in_group(group))
            .collect();
        ips.sort_unstable();
        ips.dedup();
        let live = ips.len();
        ips.retain(|ip| !ctx.is_dead(&ip.to_string()));
        if ips.len() < live {
            assert_reachable!("outage: a victim already dead is left to its own restart");
        }
        ips
    }
}

/// A uniform draw from `range`, in whole milliseconds (the restart
/// resolution); an empty range draws nothing.
fn draw_ms(range: &Range<Duration>) -> u64 {
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    crate::sim::sim_random_range_or_default(ms(range.start)..ms(range.end))
}

fn sleep_failed(error: &moonpool_core::TimeError) -> crate::SimulationError {
    crate::SimulationError::InvalidState(format!("sleep failed: {error}"))
}

#[async_trait]
impl FaultInjector for OutageInjector {
    fn name(&self) -> &'static str {
        "outage"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        if !crate::sim::sim_random_bool(self.config.probability) {
            assert_reachable!("outage: a seed draws no outage");
            return Ok(());
        }
        let start = draw_ms(&self.config.start);
        ctx.time()
            .sleep(Duration::from_millis(start))
            .await
            .map_err(|error| sleep_failed(&error))?;
        if ctx.chaos_shutdown().is_cancelled() {
            assert_reachable!("outage: the drawn start fell past the chaos window");
            return Ok(());
        }
        let victims = self.victims(ctx);
        if victims.is_empty() {
            assert_reachable!("outage: no live victim");
            return Ok(());
        }

        // Every draw at the outage instant, in IP order.
        let mut down: Vec<u64> = victims.iter().map(|_| draw_ms(&self.config.down)).collect();
        let straggler = self.config.straggler.as_ref().map(|range| {
            let at = crate::sim::sim_random_range(0..victims.len());
            down[at] = draw_ms(range);
            assert_reachable!("outage: a straggler returns last");
            at
        });

        let at = ctx.time().now();
        for (ip, down) in victims.iter().zip(&down) {
            ctx.crash_for(&ip.to_string(), Duration::from_millis(*down))?;
        }
        assert_reachable!("outage: a whole group loses power at one instant");

        // The kills run one tick from now; this timer, scheduled after them
        // for the same instant, fires once every one of them has.
        ctx.time()
            .sleep(Duration::from_nanos(1))
            .await
            .map_err(|error| sleep_failed(&error))?;
        let victims: Vec<String> = victims.iter().map(ToString::to_string).collect();
        ctx.state().publish(
            OUTAGE_STATE_KEY,
            OutageLanded {
                at,
                straggler: straggler.map(|at| victims[at].clone()),
                victims,
                down: down.into_iter().map(Duration::from_millis).collect(),
            },
        );
        Ok(())
    }
}
