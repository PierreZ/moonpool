//! Process lifecycle state for simulation runs.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use tracing::Instrument as _;

use crate::chaos::state_handle::StateHandle;
use crate::observability::SimulationLayerHandle;
use crate::runner::app_metrics::MetricsHandle;
use crate::runner::context::SimContext;
use crate::runner::fault_injector::{DeadSet, ProcessInfo};
use crate::runner::groups::GroupRegistry;
use crate::runner::locality::MachineRegistry;
use crate::runner::process::Process;
use crate::runner::tags::TagRegistry;
use crate::runner::topology::{TopologyFactory, TopologyInputs};
use crate::{assert_always_less_than_or_equal_to, assert_reachable};

/// A process factory borrowed from the builder for one iteration.
pub(crate) type ProcessFactory<'a> = &'a dyn Fn() -> Box<dyn Process>;

/// Resolved server-process configuration for one iteration.
pub(crate) struct ProcessConfig<'a> {
    /// One factory per process, parallel to `ips`. Every process of a group
    /// shares its group's factory.
    pub(crate) factories: Vec<ProcessFactory<'a>>,
    pub(crate) info: Vec<(String, String)>,
    pub(crate) ips: Vec<String>,
    pub(crate) tag_registry: TagRegistry,
    pub(crate) machine_registry: MachineRegistry,
    pub(crate) group_registry: GroupRegistry,
}

impl ProcessConfig<'_> {
    /// A process's identity within its own group: `(index, size)`.
    ///
    /// This is what a process sees as
    /// [`client_id`](SimContext::client_id) / [`client_count`](SimContext::client_count):
    /// its position among its group's members and the group's member count,
    /// so a role can number its own instances without knowing how many
    /// processes the other groups drew this seed.
    pub(crate) fn position_in_group(&self, ip: std::net::IpAddr) -> (usize, usize) {
        self.group_registry
            .position_in_group(ip)
            .expect("every configured process IP is registered in a group")
    }
}

/// Everything a restarted process needs to rebuild its [`SimContext`].
///
/// `Copy`, being nothing but shared borrows and a seed.
#[derive(Clone, Copy)]
pub(crate) struct RestartEnv<'a> {
    pub(crate) sim: &'a crate::sim::WeakSimWorld,
    pub(crate) seed: u64,
    pub(crate) state: &'a StateHandle,
    pub(crate) obs: &'a SimulationLayerHandle,
    pub(crate) metrics: &'a MetricsHandle,
    pub(crate) shutdown_signal: &'a tokio_util::sync::CancellationToken,
}

/// Owns running process tasks and their restart state.
pub(crate) struct ProcessManager<'a> {
    /// Per-process factories, parallel to `ips` (empty without processes).
    factories: Vec<ProcessFactory<'a>>,
    handles: Vec<Option<crate::executor::JoinHandle<()>>>,
    process_tokens: Vec<Option<tokio_util::sync::CancellationToken>>,
    ips: Vec<String>,
    tag_registry: TagRegistry,
    machine_registry: MachineRegistry,
    group_registry: GroupRegistry,
    all_entities: Vec<(String, String)>,
    /// The processes currently dead (killed but not yet restarted), shared
    /// with every [`FaultContext`](crate::FaultContext) so injectors can
    /// budget their kills per victim pool.
    dead: DeadSet,
}

impl<'a> ProcessManager<'a> {
    pub(crate) fn empty() -> Self {
        Self {
            factories: Vec::new(),
            handles: Vec::new(),
            process_tokens: Vec::new(),
            ips: Vec::new(),
            tag_registry: TagRegistry::default(),
            machine_registry: MachineRegistry::default(),
            group_registry: GroupRegistry::default(),
            all_entities: Vec::new(),
            dead: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    pub(crate) fn new(
        config: ProcessConfig<'a>,
        handles: Vec<Option<crate::executor::JoinHandle<()>>>,
        process_tokens: Vec<Option<tokio_util::sync::CancellationToken>>,
        all_entities: Vec<(String, String)>,
    ) -> Self {
        let ProcessConfig {
            factories,
            info: _,
            ips,
            tag_registry,
            machine_registry,
            group_registry,
        } = config;
        Self {
            factories,
            handles,
            process_tokens,
            ips,
            tag_registry,
            machine_registry,
            group_registry,
            all_entities,
            dead: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// Snapshot the process metadata needed by a fault injector.
    pub(crate) fn process_info(&self) -> ProcessInfo {
        ProcessInfo {
            process_ips: self.ips.clone(),
            tag_registry: self.tag_registry.clone(),
            machine_registry: self.machine_registry.clone(),
            group_registry: self.group_registry.clone(),
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

    pub(crate) fn abort_process(&mut self, ip: std::net::IpAddr) {
        let Some(index) = self.index_for_ip(ip) else {
            tracing::warn!(%ip, "ProcessForceKill for unknown IP");
            return;
        };
        if let Some(handle) = self.handles[index].take() {
            handle.abort();
            tracing::info!(%ip, index, "force-killed process");
        }
        self.process_tokens[index] = None;
    }

    pub(crate) fn restart(&mut self, ip: std::net::IpAddr, env: &RestartEnv<'_>) {
        let RestartEnv {
            sim,
            seed,
            state,
            obs,
            metrics,
            shutdown_signal,
        } = *env;
        let ip_string = ip.to_string();
        let Some(index) = self.index_for_ip(ip) else {
            tracing::warn!(%ip, "ProcessRestart for unknown IP");
            return;
        };
        let Some(factory) = self.factories.get(index) else {
            tracing::warn!("ProcessRestart but no process factory configured");
            return;
        };

        if let Some(handle) = self.handles[index].take() {
            handle.abort();
        }

        let process_token = shutdown_signal.child_token();
        self.process_tokens[index] = Some(process_token.clone());
        let mut process = factory();
        // A process is numbered within its own group, exactly as on first boot.
        let (client_id, client_count) = self
            .group_registry
            .position_in_group(ip)
            .unwrap_or((index, self.ips.len()));
        let topology = TopologyFactory::create_topology_with_processes(TopologyInputs {
            ip: &ip_string,
            client_id,
            client_count,
            all_entities: &self.all_entities,
            process_ips: &self.ips,
            my_tags: self.tag_registry.tags_for(ip).cloned().unwrap_or_default(),
            tag_registry: self.tag_registry.clone(),
            my_locality: self.machine_registry.locality_for(ip).cloned(),
            machine_registry: self.machine_registry.clone(),
            group_registry: self.group_registry.clone(),
            shutdown_signal: process_token,
        });
        let ctx = SimContext::new(
            crate::SimProviders::new(sim.clone(), seed, ip),
            topology,
            state.clone(),
            obs.clone(),
            metrics.clone(),
        );
        let log_ip = ip_string.clone();
        let handle = crate::executor::spawn(
            &format!("process@{ip_string}"),
            async move {
                if let Err(error) = process.run(&ctx).await {
                    tracing::debug!(%error, ip = %log_ip, "restarted process exited");
                }
            }
            .instrument(tracing::info_span!("process", ip = %ip_string)),
        );
        self.handles[index] = Some(handle);
        self.dead
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .remove(&ip);
        assert_reachable!("process_manager: process restarted");
        tracing::info!(ip = %ip_string, index, "process restarted");
    }

    pub(crate) fn abort_all(&mut self) {
        for handle in &mut self.handles {
            if let Some(handle) = handle.take() {
                handle.abort();
            }
        }
    }
}
