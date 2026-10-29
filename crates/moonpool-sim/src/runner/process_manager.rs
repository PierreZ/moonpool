//! Process lifecycle state for simulation runs.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use futures::FutureExt as _;
use tracing::Instrument as _;

use crate::chaos::state_handle::StateHandle;
use crate::observability::SimulationLayerHandle;
use crate::providers::{SimTaskProvider, TaskPanicTracker, panic_message};
use crate::runner::app_metrics::MetricsHandle;
use crate::runner::context::{ProviderBundle, ProviderEnv, ProviderFactory, SimContext};
use crate::runner::deployment::{Deployment, SpawnFactories};
use crate::runner::fault_injector::{DeadSet, ProcessInfo};
use crate::runner::groups::GroupRegistry;
use crate::runner::locality::MachineRegistry;
use crate::runner::membership::{GroupPlan, Membership};
use crate::runner::process::Process;
use crate::runner::tags::TagRegistry;
use crate::runner::topology::{TopologyFactory, TopologyInputs};
use crate::{assert_always_less_than_or_equal_to, assert_reachable};

/// A process factory borrowed from the builder for one iteration.
pub(crate) type ProcessFactory<'a, P> = &'a dyn Fn() -> Box<dyn Process<P>>;

/// Resolved server-process configuration for one iteration.
pub(crate) struct ProcessConfig<'a, P: ProviderBundle> {
    /// Each builder group's factory, by group name (groups that drew zero
    /// processes included, so a spawn can still boot one).
    pub(crate) factories: BTreeMap<String, ProcessFactory<'a, P>>,
    pub(crate) info: Vec<(String, String)>,
    pub(crate) ips: Vec<String>,
    pub(crate) tag_registry: TagRegistry,
    pub(crate) machine_registry: MachineRegistry,
    pub(crate) group_registry: GroupRegistry,
    /// How each group tags and places its next spawned process.
    pub(crate) plans: BTreeMap<String, GroupPlan>,
}

impl<P: ProviderBundle> ProcessConfig<'_, P> {
    /// The run's live membership, starting from this boot shape.
    pub(crate) fn membership(&self, max_spawns: usize) -> Membership {
        Membership::new(
            self.group_registry.clone(),
            self.tag_registry.clone(),
            self.machine_registry.clone(),
            self.plans.clone(),
            self.ips
                .iter()
                .map(|ip| ip.parse().expect("a process IP is a valid address"))
                .collect(),
            max_spawns,
        )
    }
}

/// Everything a booted or restarted process needs to build its [`SimContext`].
pub(crate) struct ProcessEnv<'a, P: ProviderBundle> {
    pub(crate) sim: &'a crate::sim::WeakSimWorld,
    pub(crate) state: &'a StateHandle,
    pub(crate) obs: &'a SimulationLayerHandle,
    pub(crate) metrics: &'a MetricsHandle,
    pub(crate) shutdown_signal: &'a tokio_util::sync::CancellationToken,
    pub(crate) membership: &'a Membership,
    pub(crate) spawn_factories: &'a SpawnFactories<P>,
    pub(crate) providers: &'a ProviderFactory<P>,
}

/// A panic that killed a process task: which process, and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessPanic {
    /// The process's IP, as the topology names it.
    pub(crate) ip: String,
    /// The panic payload, when it was a string; a placeholder otherwise.
    pub(crate) message: String,
}

/// The panics recorded across every process task of one iteration, shared
/// between the boot-time spawns and the restarts the manager performs.
pub(crate) type ProcessPanics = Arc<Mutex<Vec<ProcessPanic>>>;

/// One boot of a process: its root task, and the scope every task it spawns
/// through its provider is bound to.
///
/// Killing the boot is both halves: abort the root and cancel the scope, so
/// the descendants a crash would otherwise leave running die with it.
pub(crate) struct ProcessBoot {
    handle: crate::executor::JoinHandle<()>,
    scope: tokio_util::sync::CancellationToken,
}

impl ProcessBoot {
    /// Stop the boot: the root task now, its spawned tasks before their next
    /// poll.
    fn kill(self) {
        self.handle.abort();
        self.scope.cancel();
    }
}

/// Spawn `process` as its own task, inside the `process` span the
/// observability layer attributes events by, with `scope` as the boot's
/// task scope (the one its `ctx` providers were built with).
///
/// A panic in [`Process::run`] is caught *here*, at the one place that knows
/// which process it was, and recorded in `panics`; the seed then fails at the
/// end of the iteration ([`ProcessManager::take_panics`]). Nothing else
/// observes a process task's outcome — the manager aborts handles without
/// awaiting them — so without this a panicking process was simply a process
/// that stopped, and a run whose workloads did not notice reported success.
/// An ordinary `Err` from `run` is still an exit, not a failure: a process may
/// legitimately stop.
pub(crate) fn spawn_process<P: ProviderBundle>(
    mut process: Box<dyn Process<P>>,
    ctx: SimContext<P>,
    ip: &str,
    panics: &ProcessPanics,
    scope: tokio_util::sync::CancellationToken,
) -> ProcessBoot {
    let ip = ip.to_string();
    let span_ip = ip.clone();
    let panics = Arc::clone(panics);
    let owner: std::net::IpAddr = ip
        .parse()
        .expect("a process IP is a valid address: the manager assigned it");
    let handle = crate::executor::spawn_owned(
        &format!("process@{span_ip}"),
        owner,
        async move {
            let outcome = AssertUnwindSafe(process.run(&ctx)).catch_unwind().await;
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::debug!(%error, %ip, "process exited");
                }
                Err(payload) => {
                    let message = panic_message(payload.as_ref());
                    tracing::error!(%ip, %message, "process panicked");
                    panics
                        .lock()
                        .expect("Mutex poisoned: prior task panicked")
                        .push(ProcessPanic { ip, message });
                }
            }
        }
        .instrument(tracing::info_span!("process", ip = %span_ip)),
    );
    ProcessBoot { handle, scope }
}

/// Owns running process tasks and their restart state.
pub(crate) struct ProcessManager<'a, P: ProviderBundle> {
    /// Each builder group's factory, by group name.
    factories: BTreeMap<String, ProcessFactory<'a, P>>,
    handles: Vec<Option<ProcessBoot>>,
    process_tokens: Vec<Option<tokio_util::sync::CancellationToken>>,
    /// Every process ever booted this iteration, retired ones included, in
    /// boot order; parallel to `handles` and `process_tokens`.
    ips: Vec<String>,
    /// The run's live membership: groups, tags, localities, retirements.
    membership: Membership,
    all_entities: Vec<(String, String)>,
    /// The processes currently dead (killed but not yet restarted), shared
    /// with every [`FaultContext`](crate::FaultContext) so injectors can
    /// budget their kills per victim pool.
    dead: DeadSet,
    /// Every panic a process task of this iteration died of.
    panics: ProcessPanics,
    /// Records panics from detached tasks spawned by process providers.
    task_panics: TaskPanicTracker,
}

impl<'a, P: ProviderBundle> ProcessManager<'a, P> {
    /// A manager with no process yet: a run without `.processes()`, where a
    /// workload may still start a group mid-run.
    pub(crate) fn empty(
        membership: Membership,
        all_entities: Vec<(String, String)>,
        task_panics: TaskPanicTracker,
    ) -> Self {
        Self {
            factories: BTreeMap::new(),
            handles: Vec::new(),
            process_tokens: Vec::new(),
            ips: Vec::new(),
            membership,
            all_entities,
            dead: Arc::new(Mutex::new(BTreeSet::new())),
            panics: Arc::default(),
            task_panics,
        }
    }

    /// A manager for `config`'s processes, none of them booted yet (see
    /// [`Self::boot_all`]).
    pub(crate) fn new(
        config: ProcessConfig<'a, P>,
        membership: Membership,
        all_entities: Vec<(String, String)>,
        task_panics: TaskPanicTracker,
    ) -> Self {
        let ProcessConfig { factories, ips, .. } = config;
        Self {
            factories,
            handles: ips.iter().map(|_| None).collect(),
            process_tokens: ips.iter().map(|_| None).collect(),
            ips,
            membership,
            all_entities,
            dead: Arc::new(Mutex::new(BTreeSet::new())),
            panics: ProcessPanics::default(),
            task_panics,
        }
    }

    /// Boot every configured process, in IP order.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` if a process IP fails to parse.
    pub(crate) fn boot_all(&mut self, env: &ProcessEnv<'_, P>) -> Result<(), ()> {
        for index in 0..self.ips.len() {
            let ip: std::net::IpAddr = self.ips[index].parse().map_err(|_| ())?;
            self.boot(index, ip, env);
            tracing::debug!("Booted process {} at {}", index, self.ips[index]);
        }
        Ok(())
    }

    /// Boot a process spawned mid-run at `ip` (already active in the
    /// membership) from its group's factory.
    pub(crate) fn add(&mut self, ip: std::net::IpAddr, env: &ProcessEnv<'_, P>) {
        if self.index_for_ip(ip).is_some() {
            tracing::warn!(%ip, "ProcessSpawn for a known IP");
            return;
        }
        let index = self.ips.len();
        self.ips.push(ip.to_string());
        self.handles.push(None);
        self.process_tokens.push(None);
        self.boot(index, ip, env);
        tracing::info!(%ip, index, "spawned process booted");
    }

    /// Stop the retired process at `ip` for good: kill its boot and drop it
    /// from the dead set, since it no longer counts against any budget.
    pub(crate) fn retire(&mut self, ip: std::net::IpAddr) {
        let Some(index) = self.index_for_ip(ip) else {
            tracing::warn!(%ip, "ProcessRetire for unknown IP");
            return;
        };
        if let Some(boot) = self.handles[index].take() {
            boot.kill();
        }
        self.process_tokens[index] = None;
        self.dead
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .remove(&ip);
        tracing::info!(%ip, index, "retired process stopped for good");
    }

    /// Take the panics recorded so far, leaving the ledger empty.
    pub(crate) fn take_panics(&self) -> Vec<ProcessPanic> {
        std::mem::take(
            &mut *self
                .panics
                .lock()
                .expect("Mutex poisoned: prior task panicked"),
        )
    }

    /// Snapshot the process metadata needed by a fault injector.
    pub(crate) fn process_info(&self) -> ProcessInfo {
        ProcessInfo {
            membership: self.membership.clone(),
            dead: self.dead.clone(),
        }
    }

    fn index_for_ip(&self, ip: std::net::IpAddr) -> Option<usize> {
        let ip = ip.to_string();
        self.ips.iter().position(|candidate| candidate == &ip)
    }

    pub(crate) fn signal_graceful_shutdown(&mut self, ip: std::net::IpAddr) {
        let Some(index) = self.index_for_ip(ip) else {
            tracing::warn!(%ip, "ProcessGracefulShutdown for unknown IP");
            return;
        };
        if self.membership.is_retired(ip) {
            return;
        }
        if let Some(token) = &self.process_tokens[index] {
            token.cancel();
            let dead_count = {
                let mut dead = self
                    .dead
                    .lock()
                    .expect("Mutex poisoned: prior task panicked");
                dead.insert(ip);
                dead.len()
            };
            assert_always_less_than_or_equal_to!(
                dead_count,
                self.ips.len(),
                "dead_count <= process_count"
            );
            assert_reachable!("process_manager: graceful shutdown signaled");
            tracing::info!(%ip, index, "signaled graceful process shutdown");
        }
    }

    /// Whether a boot of the process at `ip` is live (neither killed nor
    /// finished aborting).
    pub(crate) fn is_running(&self, ip: std::net::IpAddr) -> bool {
        self.index_for_ip(ip)
            .is_some_and(|index| self.handles[index].is_some())
    }

    pub(crate) fn abort_process(&mut self, ip: std::net::IpAddr) {
        let Some(index) = self.index_for_ip(ip) else {
            tracing::warn!(%ip, "ProcessForceKill for unknown IP");
            return;
        };
        if self.membership.is_retired(ip) {
            return;
        }
        if let Some(boot) = self.handles[index].take() {
            boot.kill();
            tracing::info!(%ip, index, "force-killed process");
        }
        self.process_tokens[index] = None;
        // Dead until restarted, whoever decided the kill: a fault injector
        // marks it at the decision already, a process crashing itself
        // (`SimContext::crash_self`) does not.
        self.dead
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .insert(ip);
    }

    pub(crate) fn restart(&mut self, ip: std::net::IpAddr, env: &ProcessEnv<'_, P>) {
        let Some(index) = self.index_for_ip(ip) else {
            tracing::warn!(%ip, "ProcessRestart for unknown IP");
            return;
        };
        if self.membership.is_retired(ip) {
            assert_reachable!("deployment: a retired process is never restarted");
            return;
        }

        if let Some(boot) = self.handles[index].take() {
            boot.kill();
        }

        self.boot(index, ip, env);
        self.dead
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .remove(&ip);
        assert_reachable!("process_manager: process restarted");
        if self.membership.was_spawned(ip) {
            assert_reachable!("deployment: a process spawned mid-run restarts");
        }
        tracing::info!(ip = %ip, index, "process restarted");
    }

    /// Spawn a fresh instance of process `index` from its factory, with a new
    /// per-process token (a child of the global shutdown signal) and a new
    /// task scope.
    fn boot(&mut self, index: usize, ip: std::net::IpAddr, env: &ProcessEnv<'_, P>) {
        let ProcessEnv {
            sim,
            state,
            obs,
            metrics,
            shutdown_signal,
            membership,
            spawn_factories,
            providers,
        } = *env;
        let Some(group) = self.membership.group_for(ip) else {
            tracing::warn!(%ip, "boot of a process outside every group");
            return;
        };
        let process = if let Some(factory) = self.factories.get(&group) {
            factory()
        } else {
            let factory = spawn_factories
                .lock()
                .expect("Mutex poisoned: prior task panicked")
                .get(&group)
                .cloned();
            let Some(factory) = factory else {
                tracing::warn!(%ip, group, "boot of a process whose group has no factory");
                return;
            };
            factory()
        };
        let ip_string = ip.to_string();
        let process_token = shutdown_signal.child_token();
        let scope = tokio_util::sync::CancellationToken::new();
        self.process_tokens[index] = Some(process_token.clone());
        // A process is ranked within its own group, read live, so a role can
        // index its instances without knowing what the other groups drew.
        let topology = TopologyFactory::create_topology_with_processes(TopologyInputs {
            ip: &ip_string,
            workload_rank: None,
            all_entities: &self.all_entities,
            my_tags: self.membership.tags_for(ip).unwrap_or_default(),
            my_locality: self.membership.locality_for(ip),
            membership: self.membership.clone(),
            shutdown_signal: process_token,
        });
        let task = SimTaskProvider::default()
            .with_scope(scope.clone())
            .with_panic_reporter(self.task_panics.reporter(format!("process@{ip_string}")));
        let provider_env = ProviderEnv::new(sim.clone(), ip).with_task(task);
        let ctx = SimContext::new(
            providers(&provider_env),
            provider_env,
            topology,
            state.clone(),
            obs.clone(),
            metrics.clone(),
        )
        .with_deployment(Deployment::new(
            membership.clone(),
            sim.clone(),
            spawn_factories.clone(),
            true,
        ));
        self.handles[index] = Some(spawn_process(process, ctx, &ip_string, &self.panics, scope));
    }

    pub(crate) fn abort_all(&mut self) {
        for boot in &mut self.handles {
            if let Some(boot) = boot.take() {
                boot.kill();
            }
        }
    }
}
