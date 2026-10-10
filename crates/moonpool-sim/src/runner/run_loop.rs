//! The run loop behind [`SimulationBuilder::run`]: iteration orchestration,
//! convergence and plateau checks, exploration totals and the final report.

use super::builder::{
    ProcessEntry, SimulationBuilder, WorkloadClientInfo, WorkloadEntry, process_ip,
};
use std::collections::BTreeMap;
use std::time::Duration;

use super::wall_clock::Instant;
use tracing::instrument;

use crate::SimulationError;
use crate::observability::{SimulationLayer, SimulationLayerHandle};
use crate::providers::TaskPanicTracker;
use crate::runner::fault_injector::FaultInjector;
use crate::runner::groups::GroupRegistry;
use crate::runner::locality::MachineRegistry;
use crate::runner::process::Attrition;
use crate::runner::workload::Workload;

use super::app_metrics::SourceFactory;
use super::config::{ChaosMode, IterationControl};
use super::iteration::IterationManager;
use super::metrics::{GenerateReportInputs, MetricsCollector};
use super::orchestrator::{OrchestrateInputs, OrchestrateOutput, WorkloadOrchestrator};

/// Inputs to `run_orchestrator_blocking`.
struct RunOrchestratorInputs<'a> {
    seed: u64,
    metrics_factory: Option<&'a SourceFactory>,
    iteration_count: usize,
    workloads: Vec<Box<dyn Workload>>,
    workload_info: Vec<(String, String)>,
    client_info: Vec<WorkloadClientInfo>,
    process_config: Option<super::process_manager::ProcessConfig<'a>>,
    sim: crate::sim::SimWorld,
    fault_injectors: Vec<Box<dyn FaultInjector>>,
    chaos_duration: Option<Duration>,
    obs_handle: SimulationLayerHandle,
    run_time_budget: Duration,
}

/// Outcome of an orchestration attempt.
type OrchestrationOutcome = Result<OrchestrateOutput, (Vec<u64>, usize)>;

/// Whether an iteration let the run loop continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IterationOutcome {
    /// The seed ran to completion (successfully or not).
    Completed,
    /// The seed deadlocked: the campaign stops and reports what it gathered.
    Deadlocked,
}

/// Per-run outcomes passed into the final-report builder.
struct FinalReportInputs {
    converged: bool,
    /// The run stopped on a deadlocked seed rather than its iteration cap.
    deadlocked: bool,
    /// Saturation outcome captured during the last scan (`UntilCoverageStable`).
    saturation: Option<super::report::SaturationReport>,
    /// The exploration summary, when exploration was configured.
    exploration: Option<super::report::ExplorationReport>,
}

/// Exploration totals accumulated across seeds for the final report.
#[cfg(feature = "exploration")]
#[derive(Default)]
struct ExplorationTotals {
    timelines: u64,
    expansions: u64,
    discoveries: u64,
    bugs: u64,
    max_active_workers: usize,
    bug_recipes: Vec<super::report::BugRecipe>,
    per_seed_timelines: Vec<u64>,
}

#[cfg(feature = "exploration")]
impl ExplorationTotals {
    /// Read the controller's per-seed exploration stats and accumulate them
    /// into the run totals. Captures the bug recipes produced this seed.
    fn accumulate(&mut self, explorer: Option<&moonpool_explorer::Explorer>, seed: u64) {
        let Some(explorer) = explorer else {
            self.per_seed_timelines.push(0);
            return;
        };
        let seed_stats = explorer.seed_stats();
        self.per_seed_timelines.push(seed_stats.total_timelines);
        self.timelines += seed_stats.total_timelines;
        self.expansions += seed_stats.expansions;
        self.discoveries += seed_stats.discoveries;
        self.bugs += seed_stats.bug_found;
        self.max_active_workers = self.max_active_workers.max(seed_stats.max_active_workers);
        for recipe in explorer.bug_recipes() {
            self.bug_recipes.push(super::report::BugRecipe {
                seed,
                recipe: recipe.clone(),
            });
        }
    }

    /// Build the final `ExplorationReport` from the running totals.
    ///
    /// `sancov_edges` is `(covered, total)`, read before the explorer (which
    /// owns the sancov shared memory) is dropped.
    fn into_report(
        self,
        sancov_edges: (usize, usize),
        converged: bool,
    ) -> super::report::ExplorationReport {
        super::report::ExplorationReport {
            total_timelines: self.timelines,
            expansions: self.expansions,
            discoveries: self.discoveries,
            bugs_found: self.bugs,
            bug_recipes: self.bug_recipes,
            max_active_workers: self.max_active_workers,
            sancov_edges_covered: sancov_edges.0,
            sancov_edges_total: sancov_edges.1,
            converged,
            per_seed_timelines: self.per_seed_timelines,
        }
    }
}

/// Aggregated state passed into the convergence / plateau check helper.
struct ConvergenceState<'a> {
    iteration_control: &'a IterationControl,
    iteration_count: usize,
    reached_sometimes: &'a std::collections::BTreeSet<String>,
    all_sometimes_count: usize,
    /// Whether fork-based exploration is active (selects sancov history vs
    /// the live BSS counter reader for the code-coverage signal).
    exploration_active: bool,
    prev_signal: &'a mut usize,
    plateau_count: &'a mut usize,
    /// Captures the saturation outcome (signal source + coverage numbers).
    saturation: &'a mut Option<super::report::SaturationReport>,
    already_converged: bool,
}

impl RunState {
    /// Initialise per-run accumulators from the builder's configuration.
    fn new(builder: &SimulationBuilder) -> Self {
        let iteration_manager =
            IterationManager::new(builder.iteration_control.clone(), builder.seeds.clone());
        let progress_milestone = std::cmp::max(iteration_manager.max_iterations() / 10, 1);
        let run_id = iteration_manager.run_id();
        Self {
            iteration_manager,
            metrics_collector: MetricsCollector::new(run_id, builder.metric_queries.clone()),
            progress_milestone,
            pending_return_map: Vec::new(),
            #[cfg(feature = "exploration")]
            explorer: None,
            #[cfg(feature = "exploration")]
            exploration_totals: ExplorationTotals::default(),
            reached_sometimes: std::collections::BTreeSet::new(),
            prev_signal: 0,
            converged: false,
            plateau_count: 0,
            saturation: None,
        }
    }
}

/// Accumulated mutable state threaded through [`SimulationBuilder::run`].
struct RunState {
    iteration_manager: IterationManager,
    metrics_collector: MetricsCollector,
    /// Iteration interval at which progress is logged.
    progress_milestone: usize,
    /// Map for routing iteration-resolved workloads back to their entry slots,
    /// stashed between [`SimulationBuilder::run_orchestrator_for_iteration`]
    /// and [`SimulationBuilder::handle_orchestration_result`].
    pending_return_map: Vec<Option<usize>>,
    // Exploration state (only populated/read with the `exploration` feature).
    /// The frontier controller; lives across iterations so cumulative novelty
    /// (discovery latches, sancov history) spans the whole run.
    #[cfg(feature = "exploration")]
    explorer: Option<moonpool_explorer::Explorer>,
    #[cfg(feature = "exploration")]
    exploration_totals: ExplorationTotals,
    // Saturation tracking (`UntilCoverageStable`).
    reached_sometimes: std::collections::BTreeSet<String>,
    /// Previous progress-signal value (code edges, or reached-assertion count
    /// in the no-sancov fallback). Both signals are monotonic non-decreasing.
    prev_signal: usize,
    converged: bool,
    plateau_count: usize,
    /// Saturation outcome captured during the last scan, surfaced in the report.
    saturation: Option<super::report::SaturationReport>,
}

/// Resolved workload entries for a single iteration.
struct ResolvedEntries {
    workloads: Vec<Box<dyn Workload>>,
    /// `return_map[i] = Some(entry_idx)` means `workloads[i]` should be
    /// returned to `entries[entry_idx]` after the iteration.
    return_map: Vec<Option<usize>>,
    /// Client identity info parallel to `workloads`.
    client_info: Vec<WorkloadClientInfo>,
}

use super::report::{SimulationMetrics, SimulationReport};

impl SimulationBuilder {
    /// Resolve all entries into a flat workload list for one iteration.
    fn resolve_entries(&mut self) -> ResolvedEntries {
        let mut workloads = Vec::new();
        let mut return_map = Vec::new();
        let mut client_info = Vec::new();

        for (entry_idx, entry) in self.entries.iter_mut().enumerate() {
            match entry {
                WorkloadEntry::Instance(opt) => {
                    if let Some(w) = opt.take() {
                        return_map.push(Some(entry_idx));
                        client_info.push(WorkloadClientInfo {
                            client_id: 0,
                            client_count: 1,
                        });
                        workloads.push(w);
                    }
                }
                WorkloadEntry::Factory { count, factory } => {
                    let n = count.resolve();
                    for i in 0..n {
                        return_map.push(None);
                        client_info.push(WorkloadClientInfo {
                            client_id: i,
                            client_count: n,
                        });
                        workloads.push(factory(i));
                    }
                }
            }
        }

        ResolvedEntries {
            workloads,
            return_map,
            client_info,
        }
    }

    /// Return instance-based workloads to their entry slots after an
    /// iteration, following the `return_map` stashed in `state` when the
    /// entries were resolved.
    fn return_entries(&mut self, state: &mut RunState, workloads: Vec<Box<dyn Workload>>) {
        let return_map = std::mem::take(&mut state.pending_return_map);
        for (w, slot) in workloads.into_iter().zip(return_map) {
            if let Some(entry_idx) = slot
                && let WorkloadEntry::Instance(opt) = &mut self.entries[entry_idx]
            {
                *opt = Some(w);
            }
            // Factory-created workloads are dropped
        }
    }

    /// Spin up a fresh deterministic executor, run the orchestrator on it,
    /// and return its outcome.
    fn run_orchestrator_blocking(inputs: RunOrchestratorInputs<'_>) -> OrchestrationOutcome {
        let RunOrchestratorInputs {
            seed,
            metrics_factory,
            iteration_count,
            workloads,
            workload_info,
            client_info,
            process_config,
            sim,
            fault_injectors,
            chaos_duration,
            obs_handle,
            run_time_budget,
        } = inputs;
        // Fresh executor per iteration: dropping it cancels every task that
        // leaked past the settle phase, so no state crosses into the next seed
        // (the same contract dropping the per-iteration tokio runtime gave).
        let mut executor = crate::executor::Executor::new(seed);
        let task_panics = TaskPanicTracker::default();
        let tracker = task_panics.clone();
        let diagnostics = obs_handle.clone();
        let outcome = executor.block_on(async move {
            WorkloadOrchestrator::orchestrate_workloads(OrchestrateInputs {
                workloads,
                fault_injectors,
                obs: obs_handle,
                workload_info: &workload_info,
                client_info: &client_info,
                process_config,
                seed,
                sim,
                chaos_duration,
                metrics_factory,
                iteration_count,
                run_time_budget,
                task_panics: tracker,
            })
            .await
        });
        drop(executor);

        let panics = task_panics.take();
        for panic in &panics {
            tracing::error!(
                target: "moonpool_sim::runner",
                actor = %panic.actor,
                task = %panic.task,
                panic = %panic.message,
                "unobserved_task_panic"
            );
            diagnostics.record_task_panic(&panic.actor, &panic.task, &panic.message);
        }
        // A fatal outcome can skip the orchestrator's usual final step pass.
        // Let invariants see post-teardown diagnostics before the report is made.
        if outcome.is_err() || !panics.is_empty() {
            diagnostics.run_invariants();
        }
        if panics.is_empty() {
            return outcome;
        }
        outcome.map(|mut output| {
            output.results.extend(panics.into_iter().map(|panic| {
                Err(SimulationError::InvalidState(format!(
                    "{} task '{}' panicked: {}",
                    panic.actor, panic.task, panic.message
                )))
            }));
            output
        })
    }

    /// Build a `SimWorld` for the iteration, picking each chaos surface's config
    /// from its [`ChaosMode`]: `None` ⇒ default (off), `Random` ⇒ `random_for_seed`,
    /// `Swarm` ⇒ `swarm_for_seed`.
    ///
    /// The Swarm network subset (if any) draws from the simulation stream
    /// before the storage subset, keeping the per-seed draw order fixed and
    /// reproducible (see the draw-order contract on [`ChaosMode::resolve`]).
    /// The caller's [`NetworkFaultMask`](crate::NetworkFaultMask) and
    /// [`StorageFaultMask`](crate::StorageFaultMask) are applied
    /// afterward and consume no draws.
    pub(super) fn build_sim_for_iteration(&self, seed: u64) -> crate::sim::SimWorld {
        let network_chaos = self.network_chaos;
        let storage_chaos = self.storage_chaos;
        // Network before storage, both before the world resets the stream
        // (the draw-order contract on `ChaosMode::resolve`).
        let mut network_config = ChaosMode::resolve(
            network_chaos,
            crate::NetworkConfiguration::default,
            crate::NetworkConfiguration::random_for_seed,
            crate::NetworkConfiguration::swarm_for_seed,
        );
        let mut storage_config = ChaosMode::resolve(
            storage_chaos,
            crate::storage::StorageConfiguration::default,
            crate::storage::StorageConfiguration::random_for_seed,
            crate::storage::StorageConfiguration::swarm_for_seed,
        );
        // Buggify value-perturbation is a modifier layered on top of an enabled
        // surface — only spike knobs where chaos is actually on, so it never
        // silently switches on a fault family that wasn't enabled. Draws from
        // `SIM_RNG` (buggify is live by now; see `reset_per_iteration_state`).
        if self.buggify_knobs {
            if network_chaos.is_some() {
                network_config.chaos.apply_buggify_knobs();
            }
            if storage_chaos.is_some() {
                storage_config.apply_buggify_knobs();
            }
        }
        // A caller mask is the final fault-family decision. It consumes no RNG,
        // so adding or omitting it cannot shift config sampling or replay.
        self.network_fault_mask.apply_to(&mut network_config.chaos);
        self.storage_fault_mask.apply_to(&mut storage_config);
        // Distance latency is deployment shape, not a per-seed fault: it is
        // applied verbatim, whatever the chaos mode.
        network_config.link_latency.clone_from(&self.link_latency);
        if let Some(window) = self.tcp_send_window_bytes {
            network_config.tcp_send_window_bytes = window;
        }
        if let Some(capacity) = self.accept_backlog_capacity {
            network_config.accept_backlog_capacity = capacity;
        }
        let mut sim = crate::sim::SimWorld::new_with_network_config_and_seed(network_config, seed);
        // Unlike raw `SimWorld` use, a builder campaign has explicit phases.
        // Setup and the post-chaos quiet tail must not inherit the global sleep
        // delay fault merely because the sampled network config enabled it.
        sim.prepare_buggified_delay_campaign();
        sim.set_storage_config(storage_config);
        sim
    }

    /// Build one fresh injector per registered factory and one built-in
    /// attrition injector per resolved attrition regime. Every injector is
    /// rebuilt for every timeline and dropped with it (mirroring factory
    /// workloads); nothing comes back to the builder.
    fn collect_fault_injectors(
        fault_factories: &[Box<dyn Fn() -> Box<dyn FaultInjector> + 'static>],
        attritions: Vec<Attrition>,
        outages: Vec<crate::runner::outage::Outage>,
    ) -> Vec<Box<dyn FaultInjector>> {
        let mut fault_injectors: Vec<Box<dyn FaultInjector>> =
            fault_factories.iter().map(|factory| factory()).collect();
        for attrition in attritions {
            fault_injectors.push(Box::new(
                crate::runner::fault_injector::AttritionInjector::new(attrition),
            ));
        }
        for outage in outages {
            fault_injectors.push(Box::new(crate::runner::outage::OutageInjector::new(outage)));
        }
        fault_injectors
    }

    /// Refuse fault injectors that could never run: injectors (custom
    /// factories, attrition regimes and outages) only run inside the chaos
    /// window, so without [`Self::chaos_duration`] they would be dropped
    /// unrun while the campaign reports success. Also refuse an outage that
    /// names no registered group or carries a malformed range.
    fn validate_fault_injectors(&self) -> Result<(), SimulationError> {
        let registered: Vec<&str> = self
            .process_entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        if let Some(reason) = self
            .outages
            .iter()
            .find_map(|(outage, _)| outage.invalid(&registered))
        {
            return Err(SimulationError::InvalidConfiguration(reason));
        }
        if self.chaos_duration.is_some() {
            return Ok(());
        }
        let mut unrun = Vec::new();
        if !self.fault_factories.is_empty() {
            unrun.push(format!(
                "{} fault_factory injector(s)",
                self.fault_factories.len()
            ));
        }
        if !self.attritions.is_empty() {
            unrun.push(format!(
                "{} Chaos::Attrition regime(s)",
                self.attritions.len()
            ));
        }
        if !self.outages.is_empty() {
            unrun.push(format!("{} Chaos::Outage regime(s)", self.outages.len()));
        }
        if unrun.is_empty() {
            return Ok(());
        }
        Err(SimulationError::InvalidConfiguration(format!(
            "{} registered without SimulationBuilder::chaos_duration: fault injectors only run \
             inside the chaos window, so they would never fire; set chaos_duration",
            unrun.join(" and ")
        )))
    }

    /// Enforce the fresh-state boundary required by exploration recipes.
    ///
    /// Factory entries are reconstructed by `resolve_entries` for every root
    /// and continuation. The rejected inputs are opaque mutable values whose
    /// pristine state cannot be recovered after one timeline has run.
    pub(super) fn validate_rerun_lifecycle(&self) -> Result<(), SimulationError> {
        let feature = if self.exploration_config.is_some() {
            "exploration"
        } else if self.check_determinism {
            "check_determinism"
        } else {
            return Ok(());
        };

        if self
            .entries
            .iter()
            .any(|entry| matches!(entry, WorkloadEntry::Instance(..)))
        {
            return Err(SimulationError::InvalidConfiguration(format!(
                "{feature} runs a seed more than once and requires fresh workloads for every run; \
                 use SimulationBuilder::workload_factory or SimulationBuilder::workloads instead \
                 of SimulationBuilder::workload"
            )));
        }
        Ok(())
    }

    /// Check whether the `UntilCoverageStable` saturation condition has been
    /// met this iteration. Returns the new `converged` flag.
    ///
    /// Saturation = every observed sometimes/reachable assertion has fired AND
    /// the progress signal (real code coverage when sancov is available, else
    /// the reached-assertion count) has not grown for `plateau_seeds`
    /// consecutive seeds. Both signals are monotonic non-decreasing, so
    /// `current == prev` marks a quiet seed. A run that observes no coverage
    /// assertion at all is vacuously "all reached" and stops on the plateau.
    fn check_convergence_or_plateau(state: ConvergenceState<'_>) -> bool {
        let ConvergenceState {
            iteration_control,
            iteration_count,
            reached_sometimes,
            all_sometimes_count,
            exploration_active,
            prev_signal,
            plateau_count,
            saturation,
            already_converged,
        } = state;
        if already_converged {
            return true;
        }
        let IterationControl::UntilCoverageStable { plateau_seeds, .. } = iteration_control else {
            return false;
        };

        // Pick the progress signal: real code coverage when instrumented, else
        // the count of distinct reached sometimes/reachable slots.
        let edges = crate::chaos::exploration_glue::code_coverage_edges(exploration_active);
        let (signal, current) = match edges {
            Some(n) => (super::report::SaturationSignal::CodeCoverage, n),
            None => (
                super::report::SaturationSignal::AssertionCoverage,
                reached_sometimes.len(),
            ),
        };

        if iteration_count == 1 {
            *prev_signal = current;
        } else if current == *prev_signal {
            *plateau_count += 1;
        } else {
            *plateau_count = 0;
            *prev_signal = current;
        }

        // With no coverage assertion observed there is nothing left to reach:
        // the plateau alone decides (otherwise such a run could never converge).
        let all_reached = reached_sometimes.len() >= all_sometimes_count;

        let edges_total = crate::chaos::exploration_glue::code_coverage_total().unwrap_or_default();
        *saturation = Some(super::report::SaturationReport {
            signal,
            edges_covered: edges.unwrap_or_default(),
            edges_total,
            sometimes_hit: reached_sometimes.len(),
            sometimes_total: all_sometimes_count,
            plateau_seeds: *plateau_seeds,
        });

        tracing::warn!(
            "saturation: seed={} sometimes={}/{} signal={:?}={} quiet_seeds={}/{}",
            iteration_count,
            reached_sometimes.len(),
            all_sometimes_count,
            signal,
            current,
            *plateau_count,
            plateau_seeds,
        );
        if *plateau_count >= *plateau_seeds && all_reached {
            if all_sometimes_count == 0 {
                tracing::warn!(
                    "no assert_sometimes!/assert_reachable! was observed: saturation was judged \
                     on the {:?} plateau alone",
                    signal,
                );
            }
            tracing::info!(
                "Saturated after {} seeds: all {} sometimes reached, {:?} stable ({}) for {} seeds",
                iteration_count,
                all_sometimes_count,
                signal,
                current,
                *plateau_count,
            );
            return true;
        }
        false
    }

    /// Emit a milestone `info!` every `progress_milestone` iterations.
    fn log_progress_milestone(progress_milestone: usize, iteration_count: usize, max: usize) {
        if iteration_count.is_multiple_of(progress_milestone) {
            let iteration_f64 = super::report::usize_to_f64(iteration_count);
            let max_f64 = super::report::usize_to_f64(max);
            let pct = (iteration_f64 / max_f64) * 100.0;
            tracing::info!(
                iteration = iteration_count,
                total = max,
                "[{}/{}] {:.0}% complete",
                iteration_count,
                max,
                pct,
            );
        }
    }

    /// Reset per-iteration state: capture buffers, RNG, buggify, and chaos.
    fn reset_per_iteration_state(seed: u64, obs_handle: &SimulationLayerHandle) {
        obs_handle.reset_for_seed();
        crate::sim::reset_sim_rng();
        crate::sim::set_sim_seed(seed);
        // Route moonpool_core::select!'s branch offsets through the simulation
        // stream for this iteration.
        crate::sim::install_select_offset();
        crate::chaos::reset_always_violations();
        // Use moderate probabilities: 50% activation rate, 25% firing rate.
        crate::chaos::buggify_init(0.5);
    }

    /// Resolve the process groups into one `ProcessConfig` for the current
    /// iteration, sampling each group's count/topology/tags from the sim RNG
    /// (already seeded) in registration order. Returns `None` when no group
    /// was registered.
    ///
    /// Group *g* lands on `10.0.{g + 1}.{1..=N}` (see [`process_ip`]), so a
    /// group's members are contiguous and a single-group builder keeps its
    /// historical `10.0.1.x` addresses.
    fn resolve_process_config(
        entries: &[ProcessEntry],
    ) -> Option<super::process_manager::ProcessConfig<'_>> {
        if entries.is_empty() {
            return None;
        }
        let mut factories = Vec::new();
        let mut registry = crate::runner::tags::TagRegistry::new();
        let mut machine_registry = MachineRegistry::new();
        let mut group_registry = GroupRegistry::new();
        let mut ips = Vec::new();
        let mut info = Vec::new();
        for (group, entry) in entries.iter().enumerate() {
            let (count, localities) = entry.resolve_shape();
            let base_name = &entry.name;
            // Listed even when this seed drew zero members.
            group_registry.declare(base_name);
            for i in 0..count {
                let ip = process_ip(group, i);
                let ip_addr: std::net::IpAddr = ip.parse().expect("valid process IP");
                registry.register(ip_addr, entry.tags.resolve(i));
                if let Some(localities) = &localities {
                    machine_registry.register(ip_addr, localities[i].clone());
                }
                group_registry.register(ip_addr, base_name);
                factories.push(&*entry.factory);
                ips.push(ip.clone());
                let name = if count == 1 {
                    base_name.clone()
                } else {
                    format!("{base_name}-{i}")
                };
                info.push((name, ip));
            }
        }
        Some(super::process_manager::ProcessConfig {
            factories,
            info,
            ips,
            tag_registry: registry,
            machine_registry,
            group_registry,
        })
    }

    /// Initialise the assertion region (heap, or `MAP_SHARED` + explorer), and
    /// create the frontier controller when an exploration config is present.
    #[cfg(feature = "exploration")]
    fn init_assertions_and_exploration(
        exploration_config: Option<&crate::chaos::exploration_glue::ExplorationConfig>,
    ) -> Option<moonpool_explorer::Explorer> {
        crate::chaos::exploration_glue::init_assertion_region();
        let config = exploration_config?;
        moonpool_explorer::set_rng_count_hook(crate::sim::rng_call_count);
        match moonpool_explorer::Explorer::new(config.clone()) {
            Ok(explorer) => Some(explorer),
            Err(e) => {
                tracing::error!("Failed to initialize exploration: {}", e);
                None
            }
        }
    }

    /// Initialise the assertion region (heap table without the explorer).
    #[cfg(not(feature = "exploration"))]
    fn init_assertions_and_exploration(
        _exploration_config: Option<&crate::chaos::exploration_glue::ExplorationConfig>,
    ) {
        crate::chaos::exploration_glue::init_assertion_region();
    }

    /// Scan all assertion slots from shared memory: insert the messages of
    /// every satisfied coverage assertion into `reached`, warn for incomplete
    /// sites, and return the number of unique observed coverage contracts.
    fn scan_assertion_slots(reached: &mut std::collections::BTreeSet<String>) -> usize {
        use moonpool_assertions::AssertKind;

        let mut observed = std::collections::BTreeSet::new();
        for slot in &moonpool_assertions::assertion_read_all() {
            let Some(kind) = AssertKind::from_u8(slot.kind) else {
                continue;
            };
            if !matches!(
                kind,
                AssertKind::Sometimes
                    | AssertKind::NumericSometimes
                    | AssertKind::Reachable
                    | AssertKind::BooleanSometimesAll
            ) {
                continue;
            }
            observed.insert(slot.msg.clone());
            let satisfied = match kind {
                AssertKind::BooleanSometimesAll => {
                    slot.frontier_target > 0 && slot.frontier >= slot.frontier_target
                }
                _ => slot.pass_count > 0,
            };
            if satisfied {
                reached.insert(slot.msg.clone());
            } else if !reached.contains(&slot.msg) {
                tracing::warn!(
                    "INCOMPLETE coverage slot: kind={:?} msg={:?} pass={} fail={} frontier={}/{}",
                    kind,
                    slot.msg,
                    slot.pass_count,
                    slot.fail_count,
                    slot.frontier,
                    slot.frontier_target
                );
            }
        }
        observed.len()
    }

    /// Build the empty report returned when no workloads are registered.
    fn empty_report() -> SimulationReport {
        SimulationReport {
            iterations: 0,
            successful_runs: 0,
            failed_runs: 0,
            metrics: SimulationMetrics::default(),
            individual_metrics: Vec::new(),
            seeds_used: Vec::new(),
            seeds_failing: Vec::new(),
            assertion_results: BTreeMap::new(),
            assertion_violations: Vec::new(),
            dropped_assertion_allocations: 0,
            coverage_violations: Vec::new(),
            exploration: None,
            assertion_details: Vec::new(),
            bucket_summaries: Vec::new(),
            convergence_timeout: false,
            saturation: None,
            app_metrics: Vec::new(),
            run_id: 0,
            metric_queries: Vec::new(),
        }
    }

    #[instrument(skip_all)]
    /// Run the simulation and generate a report.
    ///
    /// Creates a fresh deterministic [`Executor`](crate::executor::Executor)
    /// per iteration for full isolation — all tasks are killed when the
    /// executor is dropped at iteration end.
    ///
    /// Failing seeds are not errors: they are in the report. An `Err` means
    /// the builder cannot run as configured, and no seed ran.
    ///
    /// # Errors
    ///
    /// Returns [`SimulationError::InvalidConfiguration`] when exploration or
    /// [`Self::check_determinism`] is combined with an instance workload (its
    /// state cannot be reconstructed for each rerun), or when fault injectors
    /// ([`Self::fault_factory`], [`Chaos::Attrition`](super::config::Chaos::Attrition)) are registered without
    /// [`Self::chaos_duration`].
    ///
    /// # Panics
    ///
    /// Panics if a simulation invariant fails or a workload panics. Also
    /// panics when a process group draws more than 255 processes, the most its
    /// `10.0.{group}.x` range can address.
    pub fn run(mut self) -> Result<SimulationReport, SimulationError> {
        self.validate_rerun_lifecycle()?;
        self.validate_fault_injectors()?;
        if self.entries.is_empty() {
            return Ok(Self::empty_report());
        }

        // Uninstall the select! offset override on every exit path (normal,
        // early return, panic): without this, the source installed by
        // install_select_offset would leak past run() and later selects on
        // this thread would silently keep drawing from the stale sim stream
        // instead of the documented entropy fallback.
        struct SelectOverrideReset;
        impl Drop for SelectOverrideReset {
            fn drop(&mut self) {
                crate::sim::uninstall_select_offset();
            }
        }
        let _select_reset = SelectOverrideReset;

        // Free the assertion region on every exit path, including a panic:
        // otherwise the next run() on this thread would inherit this run's
        // counts and discovery latches. Idempotent, so the normal path's
        // explicit cleanup in build_final_report is unaffected.
        struct AssertionRegionCleanup;
        impl Drop for AssertionRegionCleanup {
            fn drop(&mut self) {
                crate::chaos::exploration_glue::cleanup_assertion_region();
            }
        }
        let _region_cleanup = AssertionRegionCleanup;

        // Install the observability layer once for the entire run. The guard
        // is dropped when run() returns, restoring the previous subscriber.
        // All registered invariants live on the layer handle.
        let layer = SimulationLayer::new().with_level(self.trace_level);
        let (obs_handle, _obs_guard) = layer.install();
        for inv in self.invariants.drain(..) {
            obs_handle.register(inv);
        }

        #[cfg(feature = "exploration")]
        let explorer = Self::init_assertions_and_exploration(self.exploration_config.as_ref());
        #[cfg(not(feature = "exploration"))]
        Self::init_assertions_and_exploration(self.exploration_config.as_ref());

        let mut state = RunState::new(&self);
        #[cfg(feature = "exploration")]
        {
            state.explorer = explorer;
        }

        // A deadlocked seed aborts the campaign, but the report still carries
        // everything gathered so far (exploration, assertions, saturation).
        let mut deadlocked = false;
        while state.iteration_manager.should_continue() {
            if self.execute_iteration(&mut state, &obs_handle) == IterationOutcome::Deadlocked {
                deadlocked = true;
                break;
            }
            if state.converged {
                break;
            }
        }

        // Read the sancov totals while the controller (which owns the sancov
        // shared memory) is still alive, then drop it — freeing the worker
        // slots and coverage buffers. Without the `exploration` feature there
        // is no exploration data — the report's `exploration` field is simply
        // `None`, keeping the public report shape identical.
        #[cfg(feature = "exploration")]
        let exploration = {
            let sancov_edges = (
                moonpool_explorer::sancov_edges_covered(),
                moonpool_explorer::sancov_edge_count(),
            );
            state.explorer = None;
            let totals = std::mem::take(&mut state.exploration_totals);
            self.exploration_config
                .is_some()
                .then(|| totals.into_report(sancov_edges, state.converged))
        };
        #[cfg(not(feature = "exploration"))]
        let exploration = None;

        Ok(Self::build_final_report(
            state.metrics_collector,
            &state.iteration_manager,
            &self.iteration_control,
            FinalReportInputs {
                converged: state.converged,
                deadlocked,
                saturation: state.saturation,
                exploration,
            },
        ))
    }

    /// Execute one iteration of the run loop. Returns
    /// [`IterationOutcome::Deadlocked`] when the loop must stop early.
    fn execute_iteration(
        &mut self,
        state: &mut RunState,
        obs_handle: &SimulationLayerHandle,
    ) -> IterationOutcome {
        let seed = state.iteration_manager.next_iteration();
        let iteration_count = state.iteration_manager.current_iteration();

        self.prepare_iteration(obs_handle, seed, iteration_count);

        #[cfg(feature = "exploration")]
        if let Some(explorer) = state.explorer.as_mut() {
            explorer.begin_seed(seed);
        }

        let (orchestration_result, start_time) =
            self.run_orchestrator_for_iteration(state, obs_handle, seed, iteration_count);

        #[cfg(feature = "exploration")]
        let root_failed = match &orchestration_result {
            Ok(output) => Self::run_failed(output),
            Err(_) => true,
        };

        if self.handle_orchestration_result(
            state,
            orchestration_result,
            seed,
            iteration_count,
            start_time,
        ) == IterationOutcome::Deadlocked
        {
            crate::sim::stop_determinism_canary();
            // Keep the per-seed exploration series aligned with seeds_used.
            #[cfg(feature = "exploration")]
            if self.exploration_config.is_some() {
                state
                    .exploration_totals
                    .accumulate(state.explorer.as_ref(), seed);
            }
            crate::chaos::buggify_reset();
            return IterationOutcome::Deadlocked;
        }

        if self.check_determinism {
            self.run_determinism_check(state, obs_handle, seed, iteration_count);
        }

        #[cfg(feature = "exploration")]
        self.run_exploration_phase(state, obs_handle, seed, iteration_count, root_failed);

        self.finish_iteration(state, seed, iteration_count);
        IterationOutcome::Completed
    }

    /// The determinism canary's second pass (see
    /// [`SimulationBuilder::check_determinism`]): replay the seed exactly as
    /// the root run was set up, compare every draw's fingerprint against the
    /// record, and fail the iteration on the first divergence, an extra draw,
    /// or an unconsumed tail.
    fn run_determinism_check(
        &mut self,
        state: &mut RunState,
        obs_handle: &SimulationLayerHandle,
        seed: u64,
        iteration_count: usize,
    ) {
        crate::sim::begin_determinism_check();
        // The replay's assertion evaluations accumulate beside the root run's:
        // the first run's counts are part of the report and must survive.
        crate::chaos::assertions::skip_next_assertion_reset();
        Self::reset_per_iteration_state(seed, obs_handle);
        self.stage_replay_recipe();
        let (outcome, _start) =
            self.run_orchestrator_for_iteration(state, obs_handle, seed, iteration_count);
        // The replay is a full run of the seed and is judged like one: a
        // workload error, an always-violation or a deadlock fails the seed even
        // when the draw fingerprints matched.
        let replay_failure = match outcome {
            Ok(output) => {
                let errors = output.results.iter().filter(|r| r.is_err()).count();
                self.return_entries(state, output.workloads);
                if errors > 0 {
                    Some(format!(
                        "determinism canary: the replay's workloads failed ({errors} error(s))"
                    ))
                } else if crate::chaos::has_always_violations() {
                    Some("determinism canary: the replay violated an always-assertion".to_string())
                } else {
                    None
                }
            }
            Err(_) => Some("determinism canary: the replay deadlocked".to_string()),
        };
        let verdict = crate::sim::finish_determinism_check();
        let matched = verdict.is_ok();
        let detail = match &verdict {
            Ok(draws) => format!("{draws} draws replayed identically"),
            Err(violation) => violation.to_string(),
        };
        crate::assert_always!(
            matched,
            "determinism canary: replay matched the recorded draw sequence",
            { "seed" => seed, "verdict" => detail }
        );
        if !matched {
            state
                .metrics_collector
                .mark_current_iteration_failed(seed, "determinism canary: the replay diverged");
        } else if let Some(reason) = replay_failure {
            state
                .metrics_collector
                .mark_current_iteration_failed(seed, &reason);
        }
    }

    /// Run the frontier exploration loop for this seed: hand the root run's
    /// discovery journal to the controller, then execute exploration jobs
    /// through the bounded worker pool. A worker (or the controller itself
    /// with `workers == 0`) replays a job's recipe via RNG breakpoints, runs
    /// one full orchestration, and reports failure; only the controller
    /// decides what to explore next.
    #[cfg(feature = "exploration")]
    fn run_exploration_phase(
        &mut self,
        state: &mut RunState,
        obs_handle: &SimulationLayerHandle,
        seed: u64,
        iteration_count: usize,
        root_failed: bool,
    ) {
        let Some(mut explorer) = state.explorer.take() else {
            return;
        };
        explorer.observe_root_run(root_failed);
        explorer.explore(|job| {
            Self::reset_per_iteration_state(seed, obs_handle);
            // Keep the shared assertion region intact across exploration runs:
            // the discovery latches ARE the cumulative novelty. Without this,
            // SimWorld::create would zero the region and every run would
            // "re-discover" everything.
            crate::chaos::assertions::skip_next_assertion_reset();
            self.pending_replay = Some(job.recipe.clone());
            let (outcome, _start) =
                self.run_orchestrator_for_iteration(state, obs_handle, seed, iteration_count);
            match outcome {
                Ok(output) => {
                    let failed = Self::run_failed(&output);
                    // Hand workloads back so the next in-process job
                    // (workers == 0) starts from a consistent builder. In a
                    // forked worker this mutates the copy-on-write copy only.
                    self.return_entries(state, output.workloads);
                    failed
                }
                Err(_) => true,
            }
        });
        if explorer.seed_stats().bug_found > 0 {
            state
                .metrics_collector
                .mark_current_iteration_failed(seed, "exploration found a failing timeline");
        }
        state.explorer = Some(explorer);
    }

    /// Timeline replay: stage the recipe so the orchestrator installs its
    /// breakpoints once the `SimWorld`'s RNG reset has happened.
    fn stage_replay_recipe(&mut self) {
        if let Some(recipe) = &self.replay_recipe {
            self.pending_replay = Some(recipe.clone());
        }
    }

    /// Whether a completed run failed: a workload error or an `always`
    /// violation.
    #[cfg(feature = "exploration")]
    fn run_failed(output: &OrchestrateOutput) -> bool {
        output.results.iter().any(std::result::Result::is_err)
            || crate::chaos::has_always_violations()
    }

    /// Run all per-iteration setup steps before the orchestrator starts:
    /// prepare-next-seed, user hooks, reset state.
    fn prepare_iteration(
        &mut self,
        obs_handle: &SimulationLayerHandle,
        seed: u64,
        iteration_count: usize,
    ) {
        // Preserve assertion data across iterations so the final report
        // reflects all seeds, not just the last one. Under exploration this
        // also keeps the discovery latches: novelty is cumulative, so later
        // seeds only get exploration effort for genuinely new discoveries.
        if iteration_count > 1 {
            crate::chaos::assertions::skip_next_assertion_reset();
        }

        // The canary records from the first draw of the iteration (the swarm
        // subsets and process-group counts included) so the replay is held to
        // the whole run, not just the orchestrated part.
        if self.check_determinism {
            crate::sim::begin_determinism_record();
        }
        Self::reset_per_iteration_state(seed, obs_handle);

        self.stage_replay_recipe();
    }

    /// Resolve workload entries, build the per-iteration sim/fault-injectors,
    /// and drive the orchestrator. Stashes `return_map` in `state` for the
    /// subsequent result-handling step. Returns the orchestration outcome and
    /// the wall-clock start time of the orchestrator call (used for slow-seed
    /// logging).
    fn run_orchestrator_for_iteration(
        &mut self,
        state: &mut RunState,
        obs_handle: &SimulationLayerHandle,
        seed: u64,
        iteration_count: usize,
    ) -> (OrchestrationOutcome, Instant) {
        let ResolvedEntries {
            workloads,
            return_map,
            client_info,
        } = self.resolve_entries();
        state.pending_return_map = return_map;

        let workload_info: Vec<(String, String)> = workloads
            .iter()
            .enumerate()
            .map(|(i, w)| (w.name().to_string(), format!("10.0.0.{}", i + 1)))
            .collect();

        let process_config = Self::resolve_process_config(&self.process_entries);

        let mut sim = self.build_sim_for_iteration(seed);
        // `SimWorld` construction reset and reseeded the stream, so the run's
        // counted draws start here. The workload operation-alphabet swarm mask
        // is its first four draws (a fixed footprint at a fixed position);
        // without `.swarm_operations()` the reset left no mask and workloads
        // see the full alphabet.
        if self.swarm_operations {
            crate::sim::draw_swarm_op_mask();
        }
        // Exploration replay: this is the earliest point where breakpoints
        // survive until the run, and the recorded anchors count from here.
        if let Some(breakpoints) = self.pending_replay.take() {
            crate::sim::set_rng_breakpoints(breakpoints);
        }
        // Hand the engine the resolved topology as plain data so locality-aware
        // network faults and distance-based latency can see it. Empty (and thus
        // inert) for plain `.processes()` runs.
        if let Some(config) = &process_config {
            sim.set_localities(config.machine_registry.locality_map());
        }
        let start_time = Instant::now();
        // Derive the per-seed attrition regimes, in registration order: `Swarm`
        // draws a fresh reboot regime from the simulation stream (after the
        // reset and the breakpoints — the draw-order contract on
        // `ChaosMode::resolve`); `Random` uses the configured weights as
        // written. An attrition entry is always enabled, so `off` never runs.
        let attritions: Vec<Attrition> = self
            .attritions
            .iter()
            .map(|(base, mode)| {
                ChaosMode::resolve(
                    Some(*mode),
                    || base.clone(),
                    || base.clone(),
                    || {
                        base.swarm_for_seed_with_topology(
                            process_config
                                .as_ref()
                                .map(|config| &config.machine_registry),
                        )
                    },
                )
            })
            .collect();
        // Then the outages, in registration order: `Swarm` keeps or drops each
        // one (and its straggler) with two draws; `Random` keeps it as written.
        let outages: Vec<crate::runner::outage::Outage> = self
            .outages
            .iter()
            .filter_map(|(base, mode)| {
                ChaosMode::resolve(
                    Some(*mode),
                    || Some(base.clone()),
                    || Some(base.clone()),
                    || base.swarm_for_seed(),
                )
            })
            .collect();
        // The replicated fault pattern draws last, after every draw a run
        // without it makes, so opting in shifts no other surface's sample.
        if self.storage_chaos.is_some() {
            for config in &self.replicated_faults {
                let mut localities = process_config
                    .as_ref()
                    .map(|config| config.machine_registry.locality_map())
                    .unwrap_or_default();
                if let (Some(group), Some(process_config)) =
                    (config.group_name(), process_config.as_ref())
                {
                    let members = process_config.group_registry.ips_in_group(group);
                    localities.retain(|ip, _| members.contains(ip));
                }
                match sim.draw_replicated_faults(*config, &localities) {
                    crate::FaultPattern::Minority { .. } => {
                        crate::assert_reachable!("storage: a seed draws minority corruption");
                    }
                    crate::FaultPattern::Striped { .. } => {
                        crate::assert_reachable!("storage: a seed draws striped corruption");
                    }
                    crate::FaultPattern::Rolling { .. } => {
                        crate::assert_reachable!("storage: a seed draws rolling corruption");
                    }
                    crate::FaultPattern::Spared => {
                        crate::assert_reachable!("storage: too few domains, the disks are spared");
                    }
                }
            }
        }
        let fault_injectors =
            Self::collect_fault_injectors(&self.fault_factories, attritions, outages);
        let outcome = Self::run_orchestrator_blocking(RunOrchestratorInputs {
            seed,
            metrics_factory: self.metrics_factory.as_ref(),
            iteration_count,
            workloads,
            workload_info,
            client_info,
            process_config,
            sim,
            fault_injectors,
            chaos_duration: self.chaos_duration,
            obs_handle: obs_handle.clone(),
            run_time_budget: self.run_time_budget,
        });
        (outcome, start_time)
    }

    /// Process the orchestration outcome: route the success path back into
    /// state, or record the deadlocked seed as failed.
    fn handle_orchestration_result(
        &mut self,
        state: &mut RunState,
        result: OrchestrationOutcome,
        seed: u64,
        iteration_count: usize,
        start_time: Instant,
    ) -> IterationOutcome {
        let max_iterations = state.iteration_manager.max_iterations();
        match result {
            Ok(OrchestrateOutput {
                workloads: returned_workloads,
                results: all_results,
                metrics: sim_metrics,
            }) => {
                self.return_entries(state, returned_workloads);
                let wall_time = start_time.elapsed();
                state.metrics_collector.record_iteration(
                    seed,
                    wall_time,
                    &all_results,
                    crate::chaos::has_always_violations(),
                    sim_metrics,
                );
                Self::log_progress_milestone(
                    state.progress_milestone,
                    iteration_count,
                    max_iterations,
                );
                IterationOutcome::Completed
            }
            Err((faulty_seeds_from_deadlock, failed_count)) => {
                state
                    .metrics_collector
                    .add_faulty_seeds(faulty_seeds_from_deadlock);
                state.metrics_collector.add_failed_runs(failed_count);
                IterationOutcome::Deadlocked
            }
        }
    }

    /// Run all per-iteration cleanup steps after the orchestrator finished:
    /// accumulate exploration stats, run the convergence scan, reset buggify.
    fn finish_iteration(&self, state: &mut RunState, seed: u64, iteration_count: usize) {
        // `seed` is only consumed by the exploration stats accumulation below.
        #[cfg(not(feature = "exploration"))]
        let _ = seed;
        #[cfg(feature = "exploration")]
        if self.exploration_config.is_some() {
            state
                .exploration_totals
                .accumulate(state.explorer.as_ref(), seed);
        }

        let needs_assertion_scan = matches!(
            self.iteration_control,
            IterationControl::UntilCoverageStable { .. }
        );
        if needs_assertion_scan {
            let all_sometimes_count = Self::scan_assertion_slots(&mut state.reached_sometimes);
            state.converged = Self::check_convergence_or_plateau(ConvergenceState {
                iteration_control: &self.iteration_control,
                iteration_count,
                reached_sometimes: &state.reached_sometimes,
                all_sometimes_count,
                exploration_active: self.exploration_config.is_some(),
                prev_signal: &mut state.prev_signal,
                plateau_count: &mut state.plateau_count,
                saturation: &mut state.saturation,
                already_converged: state.converged,
            });
        }

        crate::chaos::buggify_reset();
    }

    /// Drain shared-memory state, free it, then build the final report.
    fn build_final_report(
        metrics_collector: MetricsCollector,
        iteration_manager: &IterationManager,
        iteration_control: &IterationControl,
        inputs: FinalReportInputs,
    ) -> SimulationReport {
        let FinalReportInputs {
            converged,
            deadlocked,
            saturation,
            exploration,
        } = inputs;

        // 1. Read assertion + bucket data (freed by cleanup/cleanup_assertions).
        let assertion_results = crate::chaos::assertion_results();
        let (assertion_violations, coverage_violations) =
            crate::chaos::validate_assertion_contracts();
        let dropped_assertion_allocations = moonpool_assertions::assertion_dropped_allocations();
        let raw_assertion_slots = moonpool_assertions::assertion_read_all();
        let raw_each_buckets = moonpool_assertions::each_bucket_read_all();

        // 2. Now safe to free the assertion region. The explorer's own shared
        // memory (worker slots, sancov buffers) was freed when the controller
        // was dropped in `run()`.
        crate::chaos::exploration_glue::cleanup_assertion_region();

        let assertion_details = build_assertion_details(&raw_assertion_slots);
        let bucket_summaries = build_bucket_summaries(&raw_each_buckets);
        let iteration_count = iteration_manager.current_iteration();

        // Detect saturation timeout: the cap was hit without saturating. A
        // deadlock stopped the run before the cap, which is not a timeout.
        let convergence_timeout = matches!(
            iteration_control,
            IterationControl::UntilCoverageStable { .. }
        ) && !converged
            && !deadlocked;

        crate::chaos::buggify_reset();

        metrics_collector.generate_report(GenerateReportInputs {
            iteration_count,
            seeds_used: iteration_manager.seeds_used().to_vec(),
            assertion_results,
            assertion_violations,
            dropped_assertion_allocations,
            coverage_violations,
            exploration,
            assertion_details,
            bucket_summaries,
            convergence_timeout,
            saturation,
        })
    }
}

/// Build [`AssertionDetail`] vec from raw assertion slot snapshots.
fn build_assertion_details(
    slots: &[moonpool_assertions::AssertionSlotSnapshot],
) -> Vec<super::report::AssertionDetail> {
    use super::report::{AssertionDetail, AssertionStatus};
    use moonpool_assertions::AssertKind;

    slots
        .iter()
        .filter_map(|slot| {
            let kind = AssertKind::from_u8(slot.kind)?;
            let total = slot.pass_count.saturating_add(slot.fail_count);

            // Skip unvisited assertions
            if total == 0 && slot.frontier == 0 {
                return None;
            }

            let status = match kind {
                AssertKind::Always
                | AssertKind::AlwaysOrUnreachable
                | AssertKind::NumericAlways => {
                    if slot.fail_count > 0 {
                        AssertionStatus::Fail
                    } else {
                        AssertionStatus::Pass
                    }
                }
                AssertKind::Sometimes | AssertKind::NumericSometimes | AssertKind::Reachable => {
                    if slot.pass_count > 0 {
                        AssertionStatus::Pass
                    } else {
                        AssertionStatus::Miss
                    }
                }
                AssertKind::Unreachable => {
                    if slot.pass_count > 0 {
                        AssertionStatus::Fail
                    } else {
                        AssertionStatus::Pass
                    }
                }
                AssertKind::BooleanSometimesAll => {
                    if slot.frontier_target > 0 && slot.frontier >= slot.frontier_target {
                        AssertionStatus::Pass
                    } else {
                        AssertionStatus::Miss
                    }
                }
            };

            Some(AssertionDetail {
                msg: slot.msg.clone(),
                kind,
                pass_count: slot.pass_count,
                fail_count: slot.fail_count,
                watermark: slot.watermark,
                frontier: slot.frontier,
                frontier_target: slot.frontier_target,
                combinations_seen: slot.combinations_seen,
                status,
            })
        })
        .collect()
}

/// Build [`BucketSiteSummary`] vec by grouping [`EachBucket`]s by site message.
fn build_bucket_summaries(
    buckets: &[moonpool_assertions::EachBucket],
) -> Vec<super::report::BucketSiteSummary> {
    use super::report::BucketSiteSummary;
    use std::collections::BTreeMap;

    let mut sites: BTreeMap<u64, BucketSiteSummary> = BTreeMap::new();

    for bucket in buckets {
        let entry = sites
            .entry(bucket.site_hash)
            .or_insert_with(|| BucketSiteSummary {
                msg: bucket.msg_str().to_string(),
                buckets_discovered: 0,
                total_hits: 0,
            });

        entry.buckets_discovered += 1;
        entry.total_hits += u64::from(bucket.pass_count);
    }

    let mut summaries: Vec<_> = sites.into_values().collect();
    summaries.sort_by_key(|s| std::cmp::Reverse(s.total_hits));
    summaries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sometimes_all_is_a_miss_until_the_full_frontier_is_reached() {
        let partial = moonpool_assertions::AssertionSlotSnapshot {
            msg: "cluster ready".to_string(),
            kind: moonpool_assertions::AssertKind::BooleanSometimesAll as u8,
            must_hit: 1,
            pass_count: 4,
            fail_count: 0,
            watermark: 0,
            combinations_seen: 3,
            frontier: 2,
            frontier_target: 3,
            conflicted: false,
        };

        let details = build_assertion_details(std::slice::from_ref(&partial));
        assert_eq!(
            details[0].status,
            crate::runner::report::AssertionStatus::Miss
        );
        assert_eq!(details[0].frontier_target, 3);
        assert_eq!(details[0].combinations_seen, 3);

        let complete = moonpool_assertions::AssertionSlotSnapshot {
            frontier: 3,
            ..partial
        };
        let details = build_assertion_details(&[complete]);
        assert_eq!(
            details[0].status,
            crate::runner::report::AssertionStatus::Pass
        );
    }
}
