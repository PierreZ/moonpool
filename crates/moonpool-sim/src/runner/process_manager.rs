//! Process lifecycle state for simulation runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::Instrument as _;

use crate::chaos::state_handle::StateHandle;
use crate::observability::SimulationLayerHandle;
use crate::runner::app_metrics::MetricsHandle;
use crate::runner::context::SimContext;
use crate::runner::fault_injector::ProcessInfo;
use crate::runner::locality::MachineRegistry;
use crate::runner::process::Process;
use crate::runner::tags::TagRegistry;
use crate::runner::topology::{TopologyFactory, TopologyInputs};
use crate::{assert_always_less_than_or_equal_to, assert_reachable};

/// Resolved server-process configuration for one iteration.
pub(crate) struct ProcessConfig<'a> {
    pub(crate) factory: &'a dyn Fn() -> Box<dyn Process>,
    pub(crate) info: Vec<(String, String)>,
    pub(crate) ips: Vec<String>,
    pub(crate) tag_registry: TagRegistry,
    pub(crate) machine_registry: MachineRegistry,
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
    factory: Option<&'a dyn Fn() -> Box<dyn Process>>,
    handles: Vec<Option<crate::executor::JoinHandle<()>>>,
    process_tokens: Vec<Option<tokio_util::sync::CancellationToken>>,
    ips: Vec<String>,
    tag_registry: TagRegistry,
    machine_registry: MachineRegistry,
    all_entities: Vec<(String, String)>,
    dead_count: Arc<AtomicUsize>,
}

impl<'a> ProcessManager<'a> {
    pub(crate) fn empty() -> Self {
        Self {
            factory: None,
            handles: Vec::new(),
            process_tokens: Vec::new(),
            ips: Vec::new(),
            tag_registry: TagRegistry::default(),
            machine_registry: MachineRegistry::default(),
            all_entities: Vec::new(),
            dead_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn new(
        factory: &'a dyn Fn() -> Box<dyn Process>,
        handles: Vec<Option<crate::executor::JoinHandle<()>>>,
        process_tokens: Vec<Option<tokio_util::sync::CancellationToken>>,
        ips: Vec<String>,
        tag_registry: TagRegistry,
        machine_registry: MachineRegistry,
        all_entities: Vec<(String, String)>,
    ) -> Self {
        Self {
            factory: Some(factory),
            handles,
            process_tokens,
            ips,
            tag_registry,
            machine_registry,
            all_entities,
            dead_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Snapshot the process metadata needed by a fault injector.
    pub(crate) fn process_info(&self) -> ProcessInfo {
        ProcessInfo {
            process_ips: self.ips.clone(),
            tag_registry: self.tag_registry.clone(),
            machine_registry: self.machine_registry.clone(),
            dead_count: self.dead_count.clone(),
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
            self.dead_count.fetch_add(1, Ordering::Relaxed);
            assert_always_less_than_or_equal_to!(
                self.dead_count.load(Ordering::Relaxed),
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
        let Some(factory) = self.factory else {
            tracing::warn!("ProcessRestart but no process factory configured");
            return;
        };

        if let Some(handle) = self.handles[index].take() {
            handle.abort();
        }

        let process_token = shutdown_signal.child_token();
        self.process_tokens[index] = Some(process_token.clone());
        let mut process = factory();
        let topology = TopologyFactory::create_topology_with_processes(TopologyInputs {
            ip: &ip_string,
            client_id: index,
            client_count: self.ips.len(),
            all_entities: &self.all_entities,
            process_ips: &self.ips,
            my_tags: self.tag_registry.tags_for(ip).cloned().unwrap_or_default(),
            tag_registry: self.tag_registry.clone(),
            my_locality: self.machine_registry.locality_for(ip).cloned(),
            machine_registry: self.machine_registry.clone(),
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
        if self.dead_count.load(Ordering::Relaxed) > 0 {
            self.dead_count.fetch_sub(1, Ordering::Relaxed);
        }
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
