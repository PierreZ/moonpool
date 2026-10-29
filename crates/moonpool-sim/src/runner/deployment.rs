//! The deployment handle: a workload changes which processes run, mid-run.
//!
//! Every workload gets one [`Deployment`] through
//! [`SimContext::deployment`](crate::SimContext::deployment). With it a
//! workload starts a new process in a group ([`spawn`](Deployment::spawn),
//! [`spawn_in`](Deployment::spawn_in)), starts a whole new group
//! ([`spawn_group`](Deployment::spawn_group)), or stops a process for good
//! ([`retire`](Deployment::retire)). An operator workload (adding machines,
//! decommissioning them) is an ordinary workload that uses this handle.
//!
//! Every change is a simulation event (`ProcessSpawn`, `ProcessRetire`),
//! scheduled one tick after the call and handled by the orchestrator next to
//! the restarts. A spawn's IP is reserved at the call, so a seed spawns at
//! the same time with the same IP on every replay, the explorer's forks and
//! the determinism canary included. No randomness is drawn.
//!
//! A process gets a handle too, but every call refuses: only a client
//! changes the deployment.
//!
//! # Usage
//!
//! ```rust
//! use moonpool_sim::{SimContext, SimulationResult};
//!
//! async fn rack_and_decommission(ctx: &SimContext) -> SimulationResult<()> {
//!     // A new machine in the "server" group, booted from the group's factory.
//!     let new_ip = ctx.deployment().spawn("server")?;
//!     // ... admit it, move work onto it, drain an old one ...
//!     if let Some(old) = ctx.topology().ips_in_group("server").first() {
//!         if *old != new_ip {
//!             ctx.deployment().retire(old)?;
//!         }
//!     }
//!     Ok(())
//! }
//! ```

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::sim::{Event, WeakSimWorld};
use crate::{SimulationError, SimulationResult, assert_reachable};

use super::context::ProviderBundle;
use super::membership::{DomainSpec, Membership};
use super::process::Process;
use crate::SimProviders;

/// A factory a workload hands to [`Deployment::spawn_group`].
pub(crate) type SharedFactory<P> = Arc<dyn Fn() -> Box<dyn Process<P>> + Send + Sync>;

/// The factories of the groups started mid-run, by group name, shared with
/// the process manager.
pub(crate) type SpawnFactories<P> = Arc<Mutex<BTreeMap<String, SharedFactory<P>>>>;

/// A workload's handle on the run's deployment (see the
/// [module documentation](self)).
#[derive(Clone)]
pub struct Deployment<P: ProviderBundle = SimProviders> {
    inner: Option<Attached<P>>,
}

#[derive(Clone)]
struct Attached<P: ProviderBundle> {
    membership: Membership,
    sim: WeakSimWorld,
    factories: SpawnFactories<P>,
    /// A process's handle: every call refuses.
    from_process: bool,
}

impl<P: ProviderBundle> std::fmt::Debug for Deployment<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deployment")
            .field("attached", &self.inner.is_some())
            .finish_non_exhaustive()
    }
}

/// Parse a process IP as the simulator addresses it.
fn parse_ip(ip: &str) -> SimulationResult<IpAddr> {
    ip.parse()
        .map_err(|e| SimulationError::InvalidState(format!("invalid IP '{ip}': {e}")))
}

impl<P: ProviderBundle> Deployment<P> {
    /// A handle attached to no run: every call refuses. What a hand-built
    /// [`SimContext`](crate::SimContext) carries.
    #[must_use]
    pub fn detached() -> Self {
        Self { inner: None }
    }

    /// A handle on `membership`, scheduling its events on `sim`.
    pub(crate) fn new(
        membership: Membership,
        sim: WeakSimWorld,
        factories: SpawnFactories<P>,
        from_process: bool,
    ) -> Self {
        Self {
            inner: Some(Attached {
                membership,
                sim,
                factories,
                from_process,
            }),
        }
    }

    /// The attached run, or why this handle refuses.
    fn attached(&self, call: &str) -> SimulationResult<&Attached<P>> {
        match &self.inner {
            None => Err(SimulationError::InvalidState(format!(
                "deployment().{call}: this context is attached to no run"
            ))),
            Some(attached) if attached.from_process => Err(SimulationError::InvalidState(format!(
                "deployment().{call}: only a workload changes the deployment, not a process"
            ))),
            Some(attached) => Ok(attached),
        }
    }

    /// Boot one new process of `group` from the group's factory, one tick
    /// from now. Returns its IP: the next free host of the group's `/24`.
    ///
    /// The process joins the topology and every victim pool when it boots.
    /// It gets the group's next round-robin tags, and its rank in the group
    /// is the next one. In a `.cluster()` group it gets a new machine in the
    /// zone of the group's highest member (see [`spawn_in`](Self::spawn_in)
    /// to choose).
    ///
    /// # Errors
    ///
    /// [`SimulationError::InvalidState`] from a process, for an unknown
    /// group, once the group's `/24` is full, or once the run's
    /// [`max_spawns`](crate::SimulationBuilder::max_spawns) cap is reached;
    /// [`SimulationError::SimulationShutdown`] once the simulation is gone.
    pub fn spawn(&self, group: &str) -> SimulationResult<String> {
        self.spawn_placed("spawn", group, None)
    }

    /// Like [`spawn`](Self::spawn), placed in a failure domain of a
    /// `.cluster()` group.
    ///
    /// # Errors
    ///
    /// As [`spawn`](Self::spawn); also for a group without a topology, or a
    /// `domain` that names an unknown machine, zone or datacenter.
    pub fn spawn_in(&self, group: &str, domain: DomainSpec) -> SimulationResult<String> {
        self.spawn_placed("spawn_in", group, Some(domain))
    }

    fn spawn_placed(
        &self,
        call: &str,
        group: &str,
        domain: Option<DomainSpec>,
    ) -> SimulationResult<String> {
        let attached = self.attached(call)?;
        let sim = attached.sim.upgrade()?;
        let ip = attached.membership.reserve(group, domain)?;
        sim.schedule_event(Event::ProcessSpawn { ip }, Duration::from_nanos(1));
        assert_reachable!("deployment: a process is spawned in the middle of a run");
        tracing::info!(%ip, group, "deployment: spawning process");
        Ok(ip.to_string())
    }

    /// Start a new process group `name` with `count` processes, booted from
    /// `factory` one tick from now. The group takes the next free `/24`, and
    /// its processes rank `0..count`. It has no tags and no topology.
    ///
    /// Later [`spawn`](Self::spawn) calls may add to it. Each process counts
    /// against the [`max_spawns`](crate::SimulationBuilder::max_spawns) cap.
    ///
    /// # Errors
    ///
    /// [`SimulationError::InvalidState`] from a process, when `name` is
    /// already a group, when 255 groups exist, or when the cap is reached
    /// (the processes reserved before that still boot).
    pub fn spawn_group(
        &self,
        name: &str,
        count: usize,
        factory: impl Fn() -> Box<dyn Process<P>> + Send + Sync + 'static,
    ) -> SimulationResult<Vec<String>> {
        let attached = self.attached("spawn_group")?;
        let sim = attached.sim.upgrade()?;
        attached.membership.declare_group(name)?;
        if let Ok(mut factories) = attached.factories.lock() {
            factories.insert(name.to_string(), Arc::new(factory));
        } else {
            return Err(SimulationError::InvalidState(
                "spawn_group: the factory table is poisoned by a prior panic".to_string(),
            ));
        }
        assert_reachable!("deployment: a new process group is started mid-run");
        let mut ips = Vec::with_capacity(count);
        for _ in 0..count {
            let ip = attached.membership.reserve(name, None)?;
            sim.schedule_event(Event::ProcessSpawn { ip }, Duration::from_nanos(1));
            ips.push(ip.to_string());
        }
        tracing::info!(group = name, count, "deployment: spawning process group");
        Ok(ips)
    }

    /// Stop the process at `ip` for good, one tick from now: it is killed,
    /// never restarted, and out of every topology query and victim pool from
    /// this call on. Its IP is never reused. Its disk stays readable
    /// ([`FaultContext::durable_bytes`](crate::FaultContext::durable_bytes))
    /// for a checker.
    ///
    /// # Errors
    ///
    /// [`SimulationError::InvalidState`] from a process, for an IP that is
    /// not a live process, or for one already retired.
    pub fn retire(&self, ip: &str) -> SimulationResult<()> {
        let attached = self.attached("retire")?;
        let sim = attached.sim.upgrade()?;
        let ip = parse_ip(ip)?;
        let group = attached.membership.retire(ip)?;
        sim.schedule_event(Event::ProcessRetire { ip }, Duration::from_nanos(1));
        assert_reachable!("deployment: a process is retired in the middle of a run");
        tracing::info!(%ip, group, "deployment: retiring process");
        Ok(())
    }

    /// The membership version: +1 on every spawned process that boots and
    /// every retire. 0 on a detached handle.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |attached| attached.membership.version())
    }
}
