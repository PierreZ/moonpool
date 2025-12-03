//! Tokio-based runner for executing workloads with real networking and timing.
//!
//! This module provides TokioRunner, which executes the same workloads as SimulationBuilder
//! but using real Tokio implementations instead of simulated ones. This validates that
//! workloads behave correctly in real-world conditions.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use crate::{SimulationResult, TokioNetworkProvider, TokioTaskProvider, TokioTimeProvider};

use super::report::SimulationMetrics;
use super::topology::WorkloadTopology;

/// Type alias for Tokio workload function signature.
type TokioWorkloadFn = Box<
    dyn Fn(
        TokioNetworkProvider,
        TokioTimeProvider,
        TokioTaskProvider,
        WorkloadTopology,
    ) -> Pin<Box<dyn Future<Output = SimulationResult<SimulationMetrics>>>>,
>;

/// A registered workload that can be executed with real Tokio providers.
pub struct TokioWorkload {
    name: String,
    ip_address: String,
    workload: TokioWorkloadFn,
}

impl fmt::Debug for TokioWorkload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokioWorkload")
            .field("name", &self.name)
            .field("ip_address", &self.ip_address)
            .field("workload", &"<closure>")
            .finish()
    }
}

/// Report generated after running workloads with TokioRunner.
#[derive(Debug, Clone)]
pub struct TokioReport {
    /// Results from each workload
    pub workload_results: Vec<(String, SimulationResult<SimulationMetrics>)>,
    /// Total wall-clock time for execution
    pub total_wall_time: Duration,
    /// Number of successful workloads
    pub successful: usize,
    /// Number of failed workloads
    pub failed: usize,
}

impl TokioReport {
    /// Calculate the success rate as a percentage.
    pub fn success_rate(&self) -> f64 {
        let total = self.successful + self.failed;
        if total == 0 {
            0.0
        } else {
            (self.successful as f64 / total as f64) * 100.0
        }
    }
}

impl fmt::Display for TokioReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Tokio Execution Report ===")?;
        writeln!(f, "Total Workloads: {}", self.successful + self.failed)?;
        writeln!(f, "Successful: {}", self.successful)?;
        writeln!(f, "Failed: {}", self.failed)?;
        writeln!(f, "Success Rate: {:.2}%", self.success_rate())?;
        writeln!(f, "Total Wall Time: {:?}", self.total_wall_time)?;
        writeln!(f)?;

        for (name, result) in &self.workload_results {
            match result {
                Ok(_) => writeln!(f, "✅ {}: SUCCESS", name)?,
                Err(e) => writeln!(f, "❌ {}: FAILED - {:?}", name, e)?,
            }
        }

        Ok(())
    }
}

/// Builder for executing workloads with real Tokio implementations.
#[derive(Debug)]
pub struct TokioRunner {
    workloads: Vec<TokioWorkload>,
    next_port: u16, // For auto-assigning ports starting from 9001
}

impl Default for TokioRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl TokioRunner {
    /// Create a new TokioRunner.
    pub fn new() -> Self {
        Self {
            workloads: Vec::new(),
            next_port: 9001, // Start from port 9001
        }
    }

    /// Register a workload with the runner.
    ///
    /// # Arguments
    /// * `name` - Name for the workload (for reporting purposes)
    /// * `workload` - Async function that takes Tokio providers and topology and returns metrics
    pub fn register_workload<S, F, Fut>(mut self, name: S, workload: F) -> Self
    where
        S: Into<String>,
        F: Fn(TokioNetworkProvider, TokioTimeProvider, TokioTaskProvider, WorkloadTopology) -> Fut
            + 'static,
        Fut: Future<Output = SimulationResult<SimulationMetrics>> + 'static,
    {
        // Auto-assign IP address with sequential port
        let ip_address = format!("127.0.0.1:{}", self.next_port);
        self.next_port += 1;

        let boxed_workload = Box::new(move |provider, time_provider, task_provider, topology| {
            let fut = workload(provider, time_provider, task_provider, topology);
            Box::pin(fut) as Pin<Box<dyn Future<Output = SimulationResult<SimulationMetrics>>>>
        });

        self.workloads.push(TokioWorkload {
            name: name.into(),
            ip_address,
            workload: boxed_workload,
        });
        self
    }

    /// Run all registered workloads and generate a report.
    pub async fn run(self) -> TokioReport {
        if self.workloads.is_empty() {
            return TokioReport {
                workload_results: Vec::new(),
                total_wall_time: Duration::ZERO,
                successful: 0,
                failed: 0,
            };
        }

        let start_time = Instant::now();

        // Create shutdown signal for this execution
        let shutdown_signal = tokio_util::sync::CancellationToken::new();

        // Build topology information for each workload
        let all_ips: Vec<String> = self
            .workloads
            .iter()
            .map(|w| w.ip_address.clone())
            .collect();

        let mut workload_results = Vec::new();
        let mut successful = 0;
        let mut failed = 0;

        // Execute workloads
        if self.workloads.len() == 1 {
            // Single workload - execute directly
            let workload = &self.workloads[0];
            let my_ip = workload.ip_address.clone();
            let peer_ips = all_ips.iter().filter(|ip| *ip != &my_ip).cloned().collect();
            let peer_names = self
                .workloads
                .iter()
                .filter(|w| w.ip_address != my_ip)
                .map(|w| w.name.clone())
                .collect();
            let topology = WorkloadTopology {
                my_ip,
                peer_ips,
                peer_names,
                shutdown_signal: shutdown_signal.clone(),
                state_registry: crate::StateRegistry::new(),
            };

            let provider = TokioNetworkProvider::new();
            let time_provider = TokioTimeProvider::new();
            let task_provider = TokioTaskProvider;

            let result =
                (workload.workload)(provider, time_provider, task_provider, topology).await;

            // For single workload, trigger shutdown signal if successful
            if result.is_ok() {
                shutdown_signal.cancel();
            }

            match result {
                Ok(_) => successful += 1,
                Err(_) => failed += 1,
            }
            workload_results.push((workload.name.clone(), result));
        } else {
            // Multiple workloads - spawn them and run concurrently
            let mut handles = Vec::new();

            for workload in &self.workloads {
                let my_ip = workload.ip_address.clone();
                let peer_ips = all_ips.iter().filter(|ip| *ip != &my_ip).cloned().collect();
                let peer_names = self
                    .workloads
                    .iter()
                    .filter(|w| w.ip_address != my_ip)
                    .map(|w| w.name.clone())
                    .collect();
                let topology = WorkloadTopology {
                    my_ip,
                    peer_ips,
                    peer_names,
                    shutdown_signal: shutdown_signal.clone(),
                    state_registry: crate::StateRegistry::new(),
                };

                let provider = TokioNetworkProvider::new();
                let time_provider = TokioTimeProvider::new();
                let task_provider = TokioTaskProvider;

                let handle = tokio::task::spawn_local((workload.workload)(
                    provider,
                    time_provider,
                    task_provider,
                    topology,
                ));
                handles.push((workload.name.clone(), handle));
            }

            // Convert handles to boxed futures with names
            let mut pending_futures: Vec<_> = handles
                .into_iter()
                .map(|(name, handle)| {
                    Box::pin(async move {
                        let result = handle.await;
                        (name, result)
                    })
                })
                .collect();

            let mut first_success_triggered = false;

            // Wait for all workloads to complete, triggering shutdown on first success
            while !pending_futures.is_empty() {
                let (completed_result, _index, remaining_futures) =
                    futures::future::select_all(pending_futures).await;

                pending_futures = remaining_futures;

                let (name, handle_result) = completed_result;
                let result = match handle_result {
                    Ok(workload_result) => workload_result,
                    Err(_) => Err(crate::SimulationError::InvalidState(
                        "Task panicked".to_string(),
                    )),
                };

                // If this is the first successful workload, trigger shutdown signal
                if !first_success_triggered && result.is_ok() {
                    tracing::debug!(
                        "TokioRunner: Workload '{}' completed successfully, triggering shutdown",
                        name
                    );
                    shutdown_signal.cancel();
                    first_success_triggered = true;
                }

                match result {
                    Ok(_) => successful += 1,
                    Err(_) => failed += 1,
                }
                workload_results.push((name, result));
            }
        }

        let total_wall_time = start_time.elapsed();

        TokioReport {
            workload_results,
            total_wall_time,
            successful,
            failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokio_runner_empty() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        let report = local_runtime.block_on(async { TokioRunner::new().run().await });

        assert_eq!(report.successful, 0);
        assert_eq!(report.failed, 0);
        assert_eq!(report.success_rate(), 0.0);
    }

    #[test]
    fn test_tokio_runner_single_workload() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        let report = local_runtime.block_on(async {
            TokioRunner::new()
                .register_workload(
                    "test_workload",
                    |_provider, _time_provider, _task_provider, _topology| async {
                        Ok(SimulationMetrics::default())
                    },
                )
                .run()
                .await
        });

        assert_eq!(report.successful, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(report.success_rate(), 100.0);
        assert!(report.total_wall_time > Duration::ZERO);
    }

    #[test]
    fn test_tokio_runner_multiple_workloads() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        let report = local_runtime.block_on(async {
            TokioRunner::new()
                .register_workload(
                    "workload1",
                    |_provider, _time_provider, _task_provider, _topology| async {
                        Ok(SimulationMetrics::default())
                    },
                )
                .register_workload(
                    "workload2",
                    |_provider, _time_provider, _task_provider, _topology| async {
                        Ok(SimulationMetrics::default())
                    },
                )
                .run()
                .await
        });

        assert_eq!(report.successful, 2);
        assert_eq!(report.failed, 0);
        assert_eq!(report.success_rate(), 100.0);
    }
}
