//! Cross-iteration metrics and final report assembly.

use std::collections::BTreeMap;
use std::time::Duration;

use moonpool_core::metrics::query::{
    MetricQueryPlan, MetricQueryReport, MetricQueryRow, MetricSnapshot,
};

use crate::chaos::AssertionStats;
use crate::{SimulationError, SimulationResult};

use super::report::{
    AssertionDetail, BucketSiteSummary, ExplorationReport, MetricAggregate, SaturationReport,
    SimulationMetrics, SimulationReport,
};

/// Collects and aggregates metrics across simulation iterations.
pub(crate) struct MetricsCollector {
    successful_runs: usize,
    failed_runs: usize,
    aggregated_metrics: SimulationMetrics,
    individual_metrics: Vec<SimulationResult<SimulationMetrics>>,
    faulty_seeds: Vec<u64>,
    /// Seeds that a stall guard stopped (each one is also faulty).
    stalled_seeds: Vec<u64>,
    /// Application-metric series folded across seeds, keyed by series identity.
    app_metrics: BTreeMap<String, MetricAggregate>,
    /// Identity of this `run()` invocation, stamped onto every query row.
    run_id: u64,
    /// Queries registered on the builder, evaluated once per successful seed.
    metric_queries: Vec<MetricQueryPlan>,
    /// Rows produced so far, parallel to `metric_queries`.
    metric_query_rows: Vec<Vec<MetricQueryRow>>,
    /// The cause of each failing seed, as the report shows it.
    failure_causes: BTreeMap<u64, String>,
    /// The seed whose successful iteration is the last entry of
    /// `individual_metrics` and whose samples are not yet folded into
    /// `app_metrics` and `metric_query_rows`. A second pass over the seed
    /// (the determinism replay, exploration) can still fail it; its samples
    /// are folded in only when the next iteration starts or the report is
    /// made.
    uncommitted_success: Option<u64>,
}

impl MetricsCollector {
    /// Create an empty metrics collector for run `run_id`, evaluating
    /// `metric_queries` against every successful iteration.
    pub(crate) fn new(run_id: u64, metric_queries: Vec<MetricQueryPlan>) -> Self {
        let metric_query_rows = vec![Vec::new(); metric_queries.len()];
        Self {
            successful_runs: 0,
            failed_runs: 0,
            aggregated_metrics: SimulationMetrics::default(),
            individual_metrics: Vec::new(),
            faulty_seeds: Vec::new(),
            stalled_seeds: Vec::new(),
            app_metrics: BTreeMap::new(),
            run_id,
            metric_queries,
            metric_query_rows,
            failure_causes: BTreeMap::new(),
            uncommitted_success: None,
        }
    }

    /// Record a simulation iteration and its assertion outcome.
    pub(crate) fn record_iteration(
        &mut self,
        seed: u64,
        wall_time: Duration,
        results: &[SimulationResult<()>],
        has_assertion_violations: bool,
        metrics: SimulationMetrics,
    ) {
        self.commit_success();
        if results
            .iter()
            .any(|result| matches!(result, Err(SimulationError::Stall(_))))
        {
            self.stalled_seeds.push(seed);
        }
        if results.iter().all(Result::is_ok) && !has_assertion_violations {
            self.record_success(seed, wall_time, metrics);
        } else {
            self.record_failure(seed, failure_cause(results, has_assertion_violations));
        }
    }

    fn record_success(&mut self, seed: u64, wall_time: Duration, metrics: SimulationMetrics) {
        self.successful_runs += 1;
        tracing::info!(seed, "simulation iteration completed");

        self.aggregated_metrics.wall_time += wall_time;
        self.aggregated_metrics.simulated_time += metrics.simulated_time;
        self.aggregated_metrics.events_processed += metrics.events_processed;

        let mut individual = metrics;
        individual.wall_time = wall_time;
        self.individual_metrics.push(Ok(individual));
        self.uncommitted_success = Some(seed);
    }

    /// Fold the samples of the last successful seed into `app_metrics` and
    /// the query rows, once no second pass can fail it any more.
    ///
    /// Only successful seeds contribute: a failed iteration's counters
    /// describe a run that did not finish, so folding them into the totals
    /// would misreport the system's steady-state behavior.
    fn commit_success(&mut self) {
        let Some(seed) = self.uncommitted_success.take() else {
            return;
        };
        let Some(Ok(metrics)) = self.individual_metrics.last() else {
            return;
        };
        MetricAggregate::absorb_samples(
            &mut self.app_metrics,
            &metrics.app_metrics,
            &metrics.app_series,
        );
        Self::evaluate_queries(
            &self.metric_queries,
            &mut self.metric_query_rows,
            self.run_id,
            seed,
            metrics,
        );
    }

    /// Evaluate every registered query against one iteration's metrics.
    ///
    /// Done here rather than in the orchestrator because this is where the
    /// seed and the iteration's metrics meet, and because only successful
    /// iterations should contribute: a run that deadlocked describes a system
    /// that never reached steady state.
    fn evaluate_queries(
        queries: &[MetricQueryPlan],
        rows: &mut [Vec<MetricQueryRow>],
        run_id: u64,
        seed: u64,
        metrics: &SimulationMetrics,
    ) {
        if queries.is_empty() {
            return;
        }
        let end_time_ms = u64::try_from(metrics.simulated_time.as_millis()).unwrap_or(u64::MAX);
        let snapshot =
            MetricSnapshot::from_run(&metrics.app_metrics, &metrics.app_series, end_time_ms);
        for (plan, rows) in queries.iter().zip(rows.iter_mut()) {
            rows.extend(plan.evaluate(&snapshot, run_id, seed));
        }
    }

    fn record_failure(&mut self, seed: u64, cause: String) {
        self.failed_runs += 1;
        tracing::error!(seed, %cause, "simulation iteration failed");
        self.individual_metrics
            .push(Err(SimulationError::InvalidState(format!(
                "seed {seed} failed: {cause}"
            ))));
        self.add_faulty_seed(seed, cause);
    }

    /// Record a seed that lost a workload task (a panic, a cancellation, or
    /// a deadlock declared by a stall guard), so the iteration has no
    /// results.
    pub(crate) fn record_lost_workload(&mut self, seed: u64) {
        self.commit_success();
        self.record_failure(
            seed,
            "a workload task was lost before it returned its workload \
             (a panic, a cancellation, or a deadlock); the workload_task_lost \
             log line names the phase and the cause"
                .to_string(),
        );
    }

    /// Add `seed` to the failing seeds with its cause; the first cause of a
    /// seed is kept.
    fn add_faulty_seed(&mut self, seed: u64, cause: String) {
        self.faulty_seeds.push(seed);
        self.failure_causes.entry(seed).or_insert(cause);
    }

    /// Reclassify the current iteration as failed after its result was
    /// recorded, with `reason` as the failure it reports.
    ///
    /// Used when a second pass over the same seed — an exploration timeline,
    /// the determinism canary's replay — finds a failure the root run did not
    /// show. That pass is part of the seed's test result, so the public
    /// iteration counts must still add up to `iterations`: one seed with a
    /// failing continuation is one failed iteration, not one successful root
    /// plus an extra failure.
    pub(crate) fn mark_current_iteration_failed(&mut self, seed: u64, reason: &str) {
        let Some(last_result) = self.individual_metrics.last_mut() else {
            return;
        };
        let Ok(metrics) = last_result else {
            return;
        };
        // The seed's samples were never folded in: drop them with it.
        self.uncommitted_success = None;

        self.successful_runs = self.successful_runs.saturating_sub(1);
        self.failed_runs += 1;
        self.aggregated_metrics.wall_time = self
            .aggregated_metrics
            .wall_time
            .saturating_sub(metrics.wall_time);
        self.aggregated_metrics.simulated_time = self
            .aggregated_metrics
            .simulated_time
            .saturating_sub(metrics.simulated_time);
        self.aggregated_metrics.events_processed = self
            .aggregated_metrics
            .events_processed
            .saturating_sub(metrics.events_processed);
        *last_result = Err(SimulationError::InvalidState(format!(
            "seed {seed} failed: {reason}"
        )));
        self.add_faulty_seed(seed, reason.to_string());
        tracing::error!(seed, "{reason}");
    }

    /// Consume the collector and assemble the public report.
    pub(crate) fn generate_report(mut self, inputs: GenerateReportInputs) -> SimulationReport {
        self.commit_success();
        let app_metrics = std::mem::take(&mut self.app_metrics)
            .into_values()
            .collect::<Vec<_>>();
        // Registration order, so the report reads the way the runner declared
        // its queries rather than in some incidental map order.
        let metric_queries = self
            .metric_queries
            .iter()
            .zip(std::mem::take(&mut self.metric_query_rows))
            .map(|(plan, rows)| MetricQueryReport::from_rows(plan, rows))
            .collect();
        SimulationReport {
            iterations: inputs.iteration_count,
            successful_runs: self.successful_runs,
            failed_runs: self.failed_runs,
            metrics: self.aggregated_metrics,
            individual_metrics: self.individual_metrics,
            seeds_used: inputs.seeds_used,
            seeds_failing: self.faulty_seeds,
            seeds_stalled: self.stalled_seeds,
            failure_causes: self.failure_causes,
            assertion_results: inputs.assertion_results,
            assertion_violations: inputs.assertion_violations,
            dropped_assertion_allocations: inputs.dropped_assertion_allocations,
            coverage_violations: inputs.coverage_violations,
            exploration: inputs.exploration,
            assertion_details: inputs.assertion_details,
            bucket_summaries: inputs.bucket_summaries,
            convergence_timeout: inputs.convergence_timeout,
            wall_clock_budget_spent: inputs.wall_clock_budget_spent,
            corpus_seeds: inputs.corpus_seeds,
            saturation: inputs.saturation,
            app_metrics,
            run_id: self.run_id,
            metric_queries,
        }
    }
}

/// The cause of a failed iteration: the text of each distinct error, in
/// result order, or the always-violation when no result failed.
fn failure_cause(results: &[SimulationResult<()>], has_assertion_violations: bool) -> String {
    let mut causes: Vec<String> = Vec::new();
    for error in results.iter().filter_map(|result| result.as_ref().err()) {
        let text = error.to_string();
        if !causes.contains(&text) {
            causes.push(text);
        }
    }
    if has_assertion_violations {
        causes.push("an always assertion was violated".to_string());
    }
    causes.join("; ")
}

/// Inputs needed to assemble a final simulation report.
pub(crate) struct GenerateReportInputs {
    pub(crate) iteration_count: usize,
    pub(crate) seeds_used: Vec<u64>,
    pub(crate) assertion_results: BTreeMap<String, AssertionStats>,
    pub(crate) assertion_violations: Vec<String>,
    pub(crate) dropped_assertion_allocations: u32,
    pub(crate) coverage_violations: Vec<String>,
    pub(crate) exploration: Option<ExplorationReport>,
    pub(crate) assertion_details: Vec<AssertionDetail>,
    pub(crate) bucket_summaries: Vec<BucketSiteSummary>,
    pub(crate) convergence_timeout: bool,
    pub(crate) wall_clock_budget_spent: bool,
    pub(crate) corpus_seeds: Vec<u64>,
    pub(crate) saturation: Option<SaturationReport>,
}
