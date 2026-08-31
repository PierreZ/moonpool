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
    /// Application-metric series folded across seeds, keyed by series identity.
    app_metrics: BTreeMap<String, MetricAggregate>,
    /// Identity of this `run()` invocation, stamped onto every query row.
    run_id: u64,
    /// Queries registered on the builder, evaluated once per successful seed.
    metric_queries: Vec<MetricQueryPlan>,
    /// Rows produced so far, parallel to `metric_queries`.
    metric_query_rows: Vec<Vec<MetricQueryRow>>,
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
            app_metrics: BTreeMap::new(),
            run_id,
            metric_queries,
            metric_query_rows,
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
        if results.iter().all(Result::is_ok) && !has_assertion_violations {
            self.record_success(seed, wall_time, metrics);
        } else {
            self.record_failure(seed);
        }
    }

    fn record_success(&mut self, seed: u64, wall_time: Duration, metrics: SimulationMetrics) {
        self.successful_runs += 1;
        tracing::info!(seed, "simulation iteration completed");

        self.aggregated_metrics.wall_time += wall_time;
        self.aggregated_metrics.simulated_time += metrics.simulated_time;
        self.aggregated_metrics.events_processed += metrics.events_processed;
        // Only successful seeds contribute: a failed iteration's counters
        // describe a run that did not finish, so folding them into the totals
        // would misreport the system's steady-state behavior.
        MetricAggregate::absorb_samples(
            &mut self.app_metrics,
            &metrics.app_metrics,
            &metrics.app_series,
        );
        self.evaluate_queries(seed, &metrics);

        let mut individual = metrics;
        individual.wall_time = wall_time;
        self.individual_metrics.push(Ok(individual));
    }

    /// Evaluate every registered query against one iteration's metrics.
    ///
    /// Done here rather than in the orchestrator because this is where the
    /// seed and the iteration's metrics meet, and because only successful
    /// iterations should contribute: a run that deadlocked describes a system
    /// that never reached steady state.
    fn evaluate_queries(&mut self, seed: u64, metrics: &SimulationMetrics) {
        if self.metric_queries.is_empty() {
            return;
        }
        let end_time_ms = u64::try_from(metrics.simulated_time.as_millis()).unwrap_or(u64::MAX);
        let snapshot =
            MetricSnapshot::from_run(&metrics.app_metrics, &metrics.app_series, end_time_ms);
        for (plan, rows) in self
            .metric_queries
            .iter()
            .zip(self.metric_query_rows.iter_mut())
        {
            rows.extend(plan.evaluate(&snapshot, self.run_id, seed));
        }
    }

    fn record_failure(&mut self, seed: u64) {
        self.failed_runs += 1;
        tracing::error!(seed, "simulation iteration failed");
        self.individual_metrics
            .push(Err(SimulationError::InvalidState(format!(
                "one or more workloads failed (seed {seed})"
            ))));
        self.faulty_seeds.push(seed);
    }

    /// Reclassify the current root iteration when one of its exploration
    /// timelines finds a bug.
    ///
    /// Exploration is part of the root seed's test result, so the public
    /// iteration counts must still add up to `iterations`: one productive
    /// seed with several failing continuations is one failed iteration, not
    /// one successful root plus several extra failures.
    #[cfg(feature = "exploration")]
    pub(crate) fn mark_current_iteration_failed_by_exploration(&mut self, seed: u64) {
        let Some(last_result) = self.individual_metrics.last_mut() else {
            return;
        };
        let Ok(metrics) = last_result else {
            return;
        };

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
            "exploration found a failing timeline (root seed {seed})"
        )));
        self.faulty_seeds.push(seed);
        tracing::error!(seed, "exploration found a failing timeline");
    }

    /// Add faulty seeds reported by an external exploration phase.
    pub(crate) fn add_faulty_seeds(&mut self, mut seeds: Vec<u64>) {
        self.faulty_seeds.append(&mut seeds);
    }

    /// Add failures reported outside the normal iteration path.
    pub(crate) fn add_failed_runs(&mut self, count: usize) {
        self.failed_runs += count;
    }

    /// Consume the collector and assemble the public report.
    pub(crate) fn generate_report(mut self, inputs: GenerateReportInputs) -> SimulationReport {
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
            assertion_results: inputs.assertion_results,
            assertion_violations: inputs.assertion_violations,
            dropped_assertion_allocations: inputs.dropped_assertion_allocations,
            coverage_violations: inputs.coverage_violations,
            exploration: inputs.exploration,
            assertion_details: inputs.assertion_details,
            bucket_summaries: inputs.bucket_summaries,
            convergence_timeout: inputs.convergence_timeout,
            saturation: inputs.saturation,
            app_metrics,
            run_id: self.run_id,
            metric_queries,
        }
    }
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
    pub(crate) saturation: Option<SaturationReport>,
}
